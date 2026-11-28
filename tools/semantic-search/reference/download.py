import sys, os
from huggingface_hub import snapshot_download
base = sys.argv[1]
specs = {
  "voyage-4-nano": ("voyageai/voyage-4-nano", "433ece5e27a1611e9360dd7134ecaaf79fb3681d", None),
  "jina-v2-base-code": ("jinaai/jina-embeddings-v2-base-code", "516f4baf13dec4ddddda8631e019b5737c8bc250",
       ["onnx/model.onnx", "tokenizer.json", "tokenizer_config.json", "config.json", "special_tokens_map.json", "README.md"]),
  "qwen3-embedding-0.6b": ("Qwen/Qwen3-Embedding-0.6B", "97b0c614be4d77ee51c0cef4e5f07c00f9eb65b3", None),
  "nomic-embed-text-v2-moe": ("nomic-ai/nomic-embed-text-v2-moe", "1066b6599d099fbb93dfcb64f9c37a7c9e503e85", None),
  "nomic-bert-2048-code": ("nomic-ai/nomic-bert-2048", "7710840340a098cfb869c4f65e87cf2b1b70caca",
       ["configuration_hf_nomic_bert.py", "modeling_hf_nomic_bert.py"]),
  "nomic-embed-code": ("nomic-ai/nomic-embed-code", "11114029805cee545ef111d5144b623787462a52", None),
}
for name in sys.argv[2:]:
    repo, rev, allow = specs[name]
    p = snapshot_download(repo, revision=rev, local_dir=os.path.join(base, name), allow_patterns=allow)
    print(name, p, flush=True)
