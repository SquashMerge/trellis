// Inference-only adaptation of huggingface/candle Qwen3 (0.11.0),
// commit 31f35b147389700ed2a178ee66a91c3cc25cc80d.
// Copyright (c) 2023-2026 The Hugging Face Inc. team.
// Licensed under Apache-2.0, one of Candle's dual MIT/Apache-2.0 licenses.
// https://github.com/huggingface/candle/blob/31f35b147389700ed2a178ee66a91c3cc25cc80d/LICENSE-APACHE
// Voyage's bidirectional mask and tokenwise projection follow
// voyageai/voyage-4-nano revision 5fb34e870f49f19e79c94d5e118f5ee730133279.

use candle_core::{D, DType, Module, Result, Tensor};
use candle_nn::{Activation, Embedding, Linear, VarBuilder};

use super::{EmbedError, EmbeddingControl};

#[derive(Debug, Clone, serde::Deserialize)]
pub(super) struct Config {
    pub(super) vocab_size: usize,
    pub(super) hidden_size: usize,
    pub(super) intermediate_size: usize,
    pub(super) num_hidden_layers: usize,
    pub(super) num_attention_heads: usize,
    pub(super) num_key_value_heads: usize,
    pub(super) head_dim: usize,
    pub(super) num_labels: usize,
    pub(super) max_position_embeddings: usize,
    pub(super) rope_theta: f64,
    pub(super) rms_norm_eps: f64,
    pub(super) hidden_act: Activation,
    pub(super) attention_bias: bool,
    pub(super) use_bidirectional_attention: bool,
    pub(super) use_sliding_window: bool,
    pub(super) rope_scaling: Option<serde_json::Value>,
}

impl Config {
    fn validate(&self) -> Result<()> {
        if self.vocab_size == 0
            || self.hidden_size == 0
            || self.intermediate_size == 0
            || self.num_hidden_layers == 0
            || self.num_attention_heads == 0
            || self.num_key_value_heads == 0
            || self.num_labels == 0
            || self.max_position_embeddings == 0
            || self.head_dim == 0
            || !self.head_dim.is_multiple_of(2)
        {
            candle_core::bail!("Qwen3 dimensions must be positive and head_dim must be even")
        }
        if !self
            .num_attention_heads
            .is_multiple_of(self.num_key_value_heads)
        {
            candle_core::bail!("Qwen3 query heads must be divisible by key/value heads")
        }
        if !self.rope_theta.is_finite()
            || self.rope_theta <= 0.
            || !self.rms_norm_eps.is_finite()
            || self.rms_norm_eps <= 0.
        {
            candle_core::bail!("Qwen3 rope_theta and rms_norm_eps must be finite and positive")
        }
        if !self.use_bidirectional_attention
            || self.use_sliding_window
            || self.rope_scaling.is_some()
        {
            candle_core::bail!("this Qwen3 port requires unscaled, full bidirectional attention")
        }
        Ok(())
    }
}

struct RmsNorm {
    weight: Tensor,
    eps: f64,
}

impl RmsNorm {
    fn new(size: usize, eps: f64, vb: VarBuilder<'_>) -> Result<Self> {
        Ok(Self {
            weight: vb.get(size, "weight")?,
            eps,
        })
    }

    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        // HF Qwen3 computes variance and normalization in f32, then casts
        // back before multiplying by the learned weight.
        let dtype = x.dtype();
        let x = x.to_dtype(DType::F32)?;
        let inv_rms = (x.sqr()?.mean_keepdim(D::Minus1)? + self.eps)?
            .sqrt()?
            .recip()?;
        x.broadcast_mul(&inv_rms)?
            .to_dtype(dtype)?
            .broadcast_mul(&self.weight)
    }
}

struct Attention {
    q_proj: Linear,
    k_proj: Linear,
    v_proj: Linear,
    o_proj: Linear,
    q_norm: RmsNorm,
    k_norm: RmsNorm,
    num_heads: usize,
    num_kv_heads: usize,
    head_dim: usize,
}

impl Attention {
    fn new(cfg: &Config, vb: VarBuilder<'_>) -> Result<Self> {
        let q_width = cfg.num_attention_heads * cfg.head_dim;
        let kv_width = cfg.num_key_value_heads * cfg.head_dim;
        Ok(Self {
            q_proj: candle_nn::linear_b(
                cfg.hidden_size,
                q_width,
                cfg.attention_bias,
                vb.pp("q_proj"),
            )?,
            k_proj: candle_nn::linear_b(
                cfg.hidden_size,
                kv_width,
                cfg.attention_bias,
                vb.pp("k_proj"),
            )?,
            v_proj: candle_nn::linear_b(
                cfg.hidden_size,
                kv_width,
                cfg.attention_bias,
                vb.pp("v_proj"),
            )?,
            o_proj: candle_nn::linear_b(
                q_width,
                cfg.hidden_size,
                cfg.attention_bias,
                vb.pp("o_proj"),
            )?,
            q_norm: RmsNorm::new(cfg.head_dim, cfg.rms_norm_eps, vb.pp("q_norm"))?,
            k_norm: RmsNorm::new(cfg.head_dim, cfg.rms_norm_eps, vb.pp("k_norm"))?,
            num_heads: cfg.num_attention_heads,
            num_kv_heads: cfg.num_key_value_heads,
            head_dim: cfg.head_dim,
        })
    }

    fn forward(&self, x: &Tensor, mask: &Tensor, cos: &Tensor, sin: &Tensor) -> Result<Tensor> {
        let (batch, tokens, _) = x.dims3()?;
        let q = self
            .q_proj
            .forward(x)?
            .reshape((batch, tokens, self.num_heads, self.head_dim))?;
        let k =
            self.k_proj
                .forward(x)?
                .reshape((batch, tokens, self.num_kv_heads, self.head_dim))?;
        let q = self.q_norm.forward(&q)?.transpose(1, 2)?.contiguous()?;
        let k = self.k_norm.forward(&k)?.transpose(1, 2)?.contiguous()?;
        let q = candle_nn::rotary_emb::rope(&q, cos, sin)?;
        let k = candle_nn::rotary_emb::rope(&k, cos, sin)?;
        let v = self
            .v_proj
            .forward(x)?
            .reshape((batch, tokens, self.num_kv_heads, self.head_dim))?
            .transpose(1, 2)?;
        let groups = self.num_heads / self.num_kv_heads;
        let k = repeat_kv(&k, groups)?.contiguous()?;
        let v = repeat_kv(&v, groups)?.contiguous()?;
        let scale = 1.0 / (self.head_dim as f64).sqrt();
        let scores = (q.matmul(&k.transpose(2, 3)?)? * scale)?;
        let scores = scores.to_dtype(DType::F32)?.broadcast_add(mask)?;
        let probs = candle_nn::ops::softmax_last_dim(&scores)?.to_dtype(x.dtype())?;
        probs
            .matmul(&v)?
            .transpose(1, 2)?
            .contiguous()?
            .reshape((batch, tokens, self.num_heads * self.head_dim))?
            .apply(&self.o_proj)
    }
}

fn repeat_kv(x: &Tensor, groups: usize) -> Result<Tensor> {
    if groups == 1 {
        return Ok(x.clone());
    }
    let (batch, heads, tokens, dim) = x.dims4()?;
    Tensor::cat(&vec![x; groups], 2)?.reshape((batch, heads * groups, tokens, dim))
}

struct Mlp {
    gate_proj: Linear,
    up_proj: Linear,
    down_proj: Linear,
    activation: Activation,
}

impl Mlp {
    fn new(cfg: &Config, vb: VarBuilder<'_>) -> Result<Self> {
        Ok(Self {
            gate_proj: candle_nn::linear_no_bias(
                cfg.hidden_size,
                cfg.intermediate_size,
                vb.pp("gate_proj"),
            )?,
            up_proj: candle_nn::linear_no_bias(
                cfg.hidden_size,
                cfg.intermediate_size,
                vb.pp("up_proj"),
            )?,
            down_proj: candle_nn::linear_no_bias(
                cfg.intermediate_size,
                cfg.hidden_size,
                vb.pp("down_proj"),
            )?,
            activation: cfg.hidden_act,
        })
    }

    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        let gate = self.gate_proj.forward(x)?.apply(&self.activation)?;
        (gate * self.up_proj.forward(x)?)?.apply(&self.down_proj)
    }
}

struct Layer {
    attention: Attention,
    mlp: Mlp,
    input_norm: RmsNorm,
    post_attention_norm: RmsNorm,
}

impl Layer {
    fn new(cfg: &Config, vb: VarBuilder<'_>) -> Result<Self> {
        Ok(Self {
            attention: Attention::new(cfg, vb.pp("self_attn"))?,
            mlp: Mlp::new(cfg, vb.pp("mlp"))?,
            input_norm: RmsNorm::new(cfg.hidden_size, cfg.rms_norm_eps, vb.pp("input_layernorm"))?,
            post_attention_norm: RmsNorm::new(
                cfg.hidden_size,
                cfg.rms_norm_eps,
                vb.pp("post_attention_layernorm"),
            )?,
        })
    }

    fn forward(&self, x: &Tensor, mask: &Tensor, cos: &Tensor, sin: &Tensor) -> Result<Tensor> {
        let attention = self
            .attention
            .forward(&self.input_norm.forward(x)?, mask, cos, sin)?;
        let x = (x + attention)?;
        let mlp = self.mlp.forward(&self.post_attention_norm.forward(&x)?)?;
        x + mlp
    }
}

pub(super) struct Model {
    embed_tokens: Embedding,
    layers: Vec<Layer>,
    norm: RmsNorm,
    linear: Linear,
    inv_freq: Tensor,
    max_positions: usize,
}

impl Model {
    pub(super) fn new(
        cfg: &Config,
        vb: VarBuilder<'_>,
        control: &EmbeddingControl<'_>,
    ) -> std::result::Result<Self, EmbedError> {
        control.check()?;
        cfg.validate()?;
        let inv_freq: Vec<f32> = (0..cfg.head_dim)
            .step_by(2)
            .map(|i| 1f32 / cfg.rope_theta.powf(i as f64 / cfg.head_dim as f64) as f32)
            .collect();
        let inv_freq = Tensor::from_vec(inv_freq, (1, cfg.head_dim / 2), vb.device())?;
        let layers = (0..cfg.num_hidden_layers)
            .map(|i| {
                control.check()?;
                let layer = Layer::new(cfg, vb.pp("model.layers").pp(i))?;
                control.check()?;
                Ok(layer)
            })
            .collect::<std::result::Result<Vec<_>, EmbedError>>()?;
        control.check()?;
        let embed_tokens =
            candle_nn::embedding(cfg.vocab_size, cfg.hidden_size, vb.pp("model.embed_tokens"))?;
        control.check()?;
        let norm = RmsNorm::new(cfg.hidden_size, cfg.rms_norm_eps, vb.pp("model.norm"))?;
        control.check()?;
        let linear = candle_nn::linear_no_bias(cfg.hidden_size, cfg.num_labels, vb.pp("linear"))?;
        control.check()?;
        Ok(Self {
            embed_tokens,
            layers,
            norm,
            linear,
            inv_freq,
            max_positions: cfg.max_position_embeddings,
        })
    }

    /// Returns tokenwise projected embeddings, not pooled sentence embeddings.
    pub(super) fn forward(
        &self,
        input_ids: &Tensor,
        attention_mask: &Tensor,
        control: &EmbeddingControl<'_>,
    ) -> std::result::Result<Tensor, EmbedError> {
        control.check()?;
        let (batch, tokens) = input_ids.dims2()?;
        if attention_mask.dims2()? != (batch, tokens)
            || batch == 0
            || tokens == 0
            || tokens > self.max_positions
        {
            return Err(EmbedError::Runtime(
                "invalid Qwen3 input and attention-mask dimensions".into(),
            ));
        }
        let device = input_ids.device();
        let padding = attention_mask.eq(0u32)?;
        let mask = padding
            .where_cond(
                &Tensor::full(f32::NEG_INFINITY, (batch, tokens), device)?,
                &Tensor::zeros((batch, tokens), DType::F32, device)?,
            )?
            .reshape((batch, 1, 1, tokens))?;
        let mut x = self.embed_tokens.forward(input_ids)?;
        let positions = Tensor::arange(0u32, tokens as u32, device)?
            .to_dtype(DType::F32)?
            .reshape((tokens, 1))?;
        let freqs = positions.matmul(&self.inv_freq)?;
        let cos = freqs.cos()?.to_dtype(x.dtype())?;
        let sin = freqs.sin()?.to_dtype(x.dtype())?;
        for layer in &self.layers {
            control.check()?;
            x = layer.forward(&x, &mask, &cos, &sin)?;
            control.check()?;
        }
        let output = self.norm.forward(&x)?.apply(&self.linear)?;
        control.check()?;
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use candle_core::Device;
    use std::collections::HashMap;

    fn toy_model() -> std::result::Result<Model, EmbedError> {
        let device = &Device::Cpu;
        let cfg = Config {
            vocab_size: 3,
            hidden_size: 2,
            intermediate_size: 2,
            num_hidden_layers: 1,
            num_attention_heads: 2,
            num_key_value_heads: 1,
            head_dim: 2,
            num_labels: 3,
            max_position_embeddings: 8,
            rope_theta: 1_000_000.,
            rms_norm_eps: 1e-6,
            hidden_act: Activation::Silu,
            attention_bias: false,
            use_bidirectional_attention: true,
            use_sliding_window: false,
            rope_scaling: None,
        };
        let mut weights = HashMap::new();
        weights.insert(
            "model.embed_tokens.weight".into(),
            Tensor::new(&[[1f32, 0.], [0., 1.], [2., -1.]], device)?,
        );
        for name in [
            "model.layers.0.input_layernorm.weight",
            "model.layers.0.post_attention_layernorm.weight",
            "model.layers.0.self_attn.q_norm.weight",
            "model.layers.0.self_attn.k_norm.weight",
            "model.norm.weight",
        ] {
            weights.insert(name.into(), Tensor::ones(2, DType::F32, device)?);
        }
        for (name, shape) in [
            ("self_attn.q_proj", (4, 2)),
            ("self_attn.k_proj", (2, 2)),
            ("mlp.gate_proj", (2, 2)),
            ("mlp.up_proj", (2, 2)),
            ("mlp.down_proj", (2, 2)),
        ] {
            weights.insert(
                format!("model.layers.0.{name}.weight"),
                Tensor::zeros(shape, DType::F32, device)?,
            );
        }
        weights.insert(
            "model.layers.0.self_attn.v_proj.weight".into(),
            Tensor::new(&[[1f32, 0.], [0., 1.]], device)?,
        );
        weights.insert(
            "model.layers.0.self_attn.o_proj.weight".into(),
            Tensor::new(&[[1f32, 0., 0., 0.], [0., 1., 0., 0.]], device)?,
        );
        weights.insert(
            "linear.weight".into(),
            Tensor::new(&[[1f32, 0.], [0., 1.], [1., 1.]], device)?,
        );
        Model::new(
            &cfg,
            VarBuilder::from_tensors(weights, DType::F32, device),
            &EmbeddingControl::new(&std::sync::atomic::AtomicBool::new(false)),
        )
    }

    #[test]
    fn future_tokens_contribute_but_padded_keys_do_not() -> std::result::Result<(), EmbedError> {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let control = EmbeddingControl::new(&cancel);
        let model = toy_model()?;
        let device = &Device::Cpu;
        let ids = Tensor::new(&[[0u32, 1], [0, 2]], device)?;
        let mask = Tensor::new(&[[1u32, 1], [1, 0]], device)?;
        let output = model.forward(&ids, &mask, &control)?;
        assert_eq!(output.dims(), &[2, 2, 3]);
        let rows = output.to_vec3::<f32>()?;
        let solo = model
            .forward(
                &Tensor::new(&[[0u32]], device)?,
                &Tensor::new(&[[1u32]], device)?,
                &control,
            )?
            .to_vec3::<f32>()?;
        assert!((rows[0][0][1] - solo[0][0][1]).abs() > 0.1);
        for (actual, expected) in rows[1][0].iter().zip(&solo[0][0]) {
            assert!((actual - expected).abs() < 1e-6);
        }
        assert!((rows[0][0][2] - rows[0][0][0] - rows[0][0][1]).abs() < 1e-6);
        assert_eq!(
            rows,
            model.forward(&ids, &mask, &control)?.to_vec3::<f32>()?
        );
        Ok(())
    }

    #[test]
    fn forward_preserves_budget_exhaustion_at_layer_boundary() -> std::result::Result<(), EmbedError>
    {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use std::time::{Duration, Instant};

        let model = toy_model()?;
        let start = Instant::now();
        let checks = Arc::new(AtomicUsize::new(0));
        let clock_checks = checks.clone();
        let budget = super::super::QueryBudget::new(
            start + Duration::from_secs(1),
            Arc::new(move || {
                if clock_checks.fetch_add(1, Ordering::Relaxed) >= 2 {
                    start + Duration::from_secs(1)
                } else {
                    start
                }
            }),
        );
        let cancel = AtomicBool::new(false);
        let result = model.forward(
            &Tensor::new(&[[0u32]], &Device::Cpu)?,
            &Tensor::new(&[[1u32]], &Device::Cpu)?,
            &EmbeddingControl::with_budget(&cancel, &budget),
        );
        assert!(matches!(result, Err(EmbedError::BudgetExceeded)));
        assert_eq!(checks.load(Ordering::Relaxed), 3);
        assert!(!cancel.load(Ordering::Acquire));
        Ok(())
    }

    #[test]
    fn asymmetric_attention_matches_scalar_qk_rope_and_grouping() -> Result<()> {
        let device = &Device::Cpu;
        let matrix = |rows: usize, kind: usize| -> Vec<f32> {
            (0..rows * 3)
                .map(|i| {
                    let (r, c) = (i / 3, i % 3);
                    match kind {
                        0 => ((r * 7 + c * 11 + 3) % 19) as f32 / 7. - 9. / 7.,
                        1 => ((r * 13 + c * 5 + 2) % 23) as f32 / 9. - 11. / 9.,
                        _ => ((r * 3 + c * 7) % 17) as f32 / 5. - 8. / 5.,
                    }
                })
                .collect()
        };
        let (qw, kw, vw) = (matrix(16, 0), matrix(8, 1), matrix(8, 2));
        let q_gain = [1f32, 0.5, 1.5, 0.75];
        let k_gain = [0.7f32, 1.3, 0.9, 1.1];
        let projection = |w: &[f32], rows| -> Result<Linear> {
            Ok(Linear::new(Tensor::from_slice(w, (rows, 3), device)?, None))
        };
        let identity: Vec<f32> = (0..256)
            .map(|i| if i / 16 == i % 16 { 1. } else { 0. })
            .collect();
        let attention = Attention {
            q_proj: projection(&qw, 16)?,
            k_proj: projection(&kw, 8)?,
            v_proj: projection(&vw, 8)?,
            o_proj: Linear::new(Tensor::from_vec(identity, (16, 16), device)?, None),
            q_norm: RmsNorm {
                weight: Tensor::new(&q_gain, device)?,
                eps: 1e-6,
            },
            k_norm: RmsNorm {
                weight: Tensor::new(&k_gain, device)?,
                eps: 1e-6,
            },
            num_heads: 4,
            num_kv_heads: 2,
            head_dim: 4, // Not hidden_size/num_heads: input width is three.
        };
        let x = Tensor::new(&[[[1f32, 0., 0.], [0., 1., 0.], [0., 0., 1.]]], device)?;
        let mask = Tensor::new(&[[[[0f32, 0., f32::NEG_INFINITY]]]], device)?;
        let angles: Vec<f32> = (0..3)
            .flat_map(|position| [position as f32, position as f32 * 0.001])
            .collect();
        let cos: Vec<f32> = angles.iter().map(|a| a.cos()).collect();
        let sin: Vec<f32> = angles.iter().map(|a| a.sin()).collect();
        let actual = attention
            .forward(
                &x,
                &mask,
                &Tensor::from_vec(cos, (3, 2), device)?,
                &Tensor::from_vec(sin, (3, 2), device)?,
            )?
            .to_vec3::<f32>()?;

        let normalized_rotated = |weights: &[f32], gain: &[f32; 4], head, position| {
            let mut values = [0f64; 4];
            for (d, value) in values.iter_mut().enumerate() {
                *value = f64::from(weights[(head * 4 + d) * 3 + position]);
            }
            let inv_rms = (values.iter().map(|v| v * v).sum::<f64>() / 4. + 1e-6)
                .sqrt()
                .recip();
            for d in 0..4 {
                values[d] *= inv_rms * f64::from(gain[d]);
            }
            let mut rotated = [0f64; 4];
            for d in 0..2 {
                let angle = position as f64 / 1_000_000f64.powf(d as f64 / 2.);
                let (s, c) = angle.sin_cos();
                rotated[d] = values[d] * c - values[d + 2] * s;
                rotated[d + 2] = values[d + 2] * c + values[d] * s;
            }
            rotated
        };
        for (query_position, actual_position) in actual[0].iter().enumerate() {
            for head in 0..4 {
                let q = normalized_rotated(&qw, &q_gain, head, query_position);
                let kv_head = head / 2; // Heads 0,1 -> KV0; heads 2,3 -> KV1.
                let mut logits = [0f64; 2]; // Position 2 is padding, never a key.
                for (key_position, logit) in logits.iter_mut().enumerate() {
                    let k = normalized_rotated(&kw, &k_gain, kv_head, key_position);
                    *logit = (0..4).map(|d| q[d] * k[d]).sum::<f64>() / 4f64.sqrt();
                }
                let max = logits[0].max(logits[1]);
                let exp = [(logits[0] - max).exp(), (logits[1] - max).exp()];
                for d in 0..4 {
                    let row = (kv_head * 4 + d) * 3;
                    let expected = (exp[0] * f64::from(vw[row]) + exp[1] * f64::from(vw[row + 1]))
                        / (exp[0] + exp[1]);
                    let observed = f64::from(actual_position[head * 4 + d]);
                    assert!(
                        (observed - expected).abs() < 2e-5,
                        "query={query_position} head={head} dim={d}: {observed} != {expected}"
                    );
                }
            }
        }
        Ok(())
    }
}
