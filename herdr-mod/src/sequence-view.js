// The Sequence tab: the followed file's methods, one method's static call sequence, its drawing and
// its steps as rows. These are methods of the pane (main.js installs them on its class unchanged);
// the drawing itself is sequence.js.
import { div, image } from "gpui-kit";
import { v_flex, h_flex } from "gpui-base";
import { theme } from "herdr";
import { TrellisError, samePin } from "./client.js";
import { drawSequence, helpers as sequenceHelpers, targetOf } from "./sequence.js";
import { MAX_STEP_ROWS, MAX_HISTORY, DRAW_TIMEOUT_MS, yieldTo, drawTimeout, fail, settingsMono } from "./common.js";

export class SequenceView {
  // --- Sequence ---

  // Follows the Files selection to that file's methods; the first consequential named method
  // opens unless one is open already for this file.
  followSequence(file, cx) {
    const path = file?.checkout?.rootKey === this.rootKey ? file.path : null;
    if (!path || !this.status?.pin || this.seq.file === path) return;
    this.seq = { file: path, history: [], expanded: new Set() };
    this.loadMethods(cx);
  }

  // A new revision reloads what the pane shows and, during an agent show, what Back returns to:
  // a sequence is never shown under another revision's header (#824).
  reloadSequence(cx) {
    const back = this.shown?.back?.seq;
    if (back?.seed && back !== this.seq) this.loadSequence(back.seed, false, cx, back, "reload");
    if (!this.seq?.file) {
      if (this.seq?.seed) this.loadSequence(this.seq.seed, false, cx, this.seq, "reload");
      return;
    }
    const keep = this.seq.seed;
    this.seq = { ...this.seq, methods: null, methodsError: null, data: null, error: null, drawn: null, drawError: null, keep };
    this.seqTried = null;
    this.loadMethods(cx);
  }

  // The file's method list. Its first consequential method opens unless a choice was made
  // while it was read; a caller chosen from Deviations (#939) is kept, and read again here
  // after a revision change, even when it isn't in this file's list.
  loadMethods(cx) {
    const { rootKey, serial, client } = this, pin = this.status?.pin, seq = this.seq, run = {}, picks = seq.picks;
    seq.methodsRun = run;
    const live = () => serial === this.serial && seq === this.seq && run === seq.methodsRun;
    cx.spawn(async (cx) => {
      try {
        const data = await client.methods(rootKey, pin, seq.file);
        if (!live()) return;
        if (!Array.isArray(data?.items)) throw new TrellisError("invalid_response", "Invalid method list. Nothing displayed.");
        seq.methods = data.items.map((item) => item?.symbol).filter((symbol) => typeof symbol?.id === "string" && typeof symbol.name === "string");
        const named = seq.methods.filter((m) => !m.name.startsWith("<"));
        if (seq.picks !== picks) return;
        if (seq.caller && seq.keep) return this.loadSequence(seq.keep, false, cx, seq, "reload");
        const first = seq.methods.find((m) => m.id === seq.keep?.id) || named.find((m) => data.items.find((i) => i.symbol === m)?.consequential) || named[0] || seq.methods[0];
        if (first) this.loadSequence(first, false, cx, seq, "auto");
      } catch (error) {
        if (!live()) return;
        seq.methodsError = fail(error);
      } finally { cx.notify(); }
    });
  }

  // Opens one method's sequence; `push` keeps the current one for Back (a re-center).
  // `seq` is the shown sequence, or the one an agent show's Back returns to. `how` is "auto"
  // (the method list's own pick), "reload" (the same seed at a new revision), "caller" (a
  // Deviations line's caller, which owns the selection until another pick), or a pick.
  loadSequence(symbol, push, cx, seq = this.seq, how = "pick") {
    const { rootKey, serial, client } = this, pin = this.status?.pin, run = {};
    // A pick or a caller outranks a method list still being read; a reload keeps the owner.
    if (how === "pick" || how === "caller") { seq.picks = (seq.picks || 0) + 1; seq.caller = how === "caller"; }
    if (push && seq.seed) seq.history = [...seq.history, seq.seed].slice(-MAX_HISTORY);
    Object.assign(seq, { seed: { id: symbol.id, name: symbol.name, path: symbol.path }, data: null, drawn: null, error: null, selected: null, expanded: new Set(), loading: true, run });
    // Only the newest sequence request of this view may set data, an error or loading.
    const live = () => serial === this.serial && (seq === this.seq || seq === this.shown?.back?.seq) && run === seq.run;
    cx.spawn(async (cx) => {
      try {
        const data = await client.sequence(rootKey, pin, symbol.id);
        if (!live()) return;
        // As the browser checks it: the answer is for this revision and this seed, whole.
        if (!samePin(data?.revision, pin) || data?.seed?.id !== symbol.id || !Array.isArray(data.steps) || !Array.isArray(data.participants)) {
          throw new TrellisError("revision_conflict", "Sequence provenance mismatch. Nothing displayed.");
        }
        seq.data = data; seq.seed = { id: data.seed.id, name: data.seed.name, path: data.seed.path };
      } catch (error) {
        if (!live()) return;
        seq.error = fail(error);
      } finally { if (live()) seq.loading = false; cx.notify(); }
    });
  }

  scheduleSequenceDraw(cx) {
    const seq = this.seq, data = seq?.data;
    if (!data || this.seqDrawing) return;
    const t = theme();
    const key = JSON.stringify([data.revision, data.seed.id, seq.selected, [...seq.expanded], t.background, t.foreground, t.accent, t.selected]);
    if (this.seqTried === key) return;
    this.seqDrawing = true; this.seqTried = key;
    const serial = this.serial, run = {};
    this.seqRun = run;
    const watchdog = cx.timer.after(DRAW_TIMEOUT_MS, (cx) => {
      if (this.seqRun !== run) return;
      this.seqRun = null; this.seqDrawing = false;
      if (serial === this.serial && seq === this.seq && data === seq.data) { seq.drawn = null; seq.drawError = drawTimeout("Sequence"); }
      cx.notify();
    });
    const live = () => this.seqRun === run && serial === this.serial && seq === this.seq && data === seq.data;
    cx.spawn(async (cx) => {
      try {
        const pause = yieldTo(cx);
        await pause();
        if (!live()) return;
        const out = await drawSequence(data, { theme: t, selectedId: seq.selected, expanded: seq.expanded, pause });
        if (!live()) return;
        const asset = out.svg ? await this.saveDrawing("sequence", out.svg) : null;
        if (!live()) return;
        seq.drawn = asset ? { ...out, data, asset } : { ...out, data };
        seq.drawError = null;
      } catch (error) {
        if (!live()) return;
        seq.drawn = null; seq.drawError = fail(error);
      } finally {
        watchdog.cancel();
        if (this.seqRun === run) { this.seqRun = null; this.seqDrawing = false; }
        cx.notify();
      }
    });
  }

  // A step: select it; a call with a proven target re-centers the view on that method, as the
  // browser's target action does; a collapsed call chain opens or closes.
  activateStep(step, cx) {
    const seq = this.seq, data = seq.data;
    if (!step || !data) return;
    const target = targetOf(data, step);
    if (target) { this.loadSequence({ id: target.declaration.id, name: target.declaration.name, path: target.declaration.path }, true, cx); cx.notify(); return; }
    if (step.kind === "group") { if (seq.expanded.has(step.id)) seq.expanded.delete(step.id); else seq.expanded.add(step.id); }
    seq.selected = step.id;
    cx.notify();
  }

  pressSequence(event, cx) {
    const drawn = this.seq?.drawn, at = event?.local_position;
    if (!drawn?.boxes || !at) return;
    const box = drawn.boxes.filter((b) => at.x >= b.x && at.x <= b.x + b.w && at.y >= b.y && at.y <= b.y + b.h).sort((a, b) => a.w * a.h - b.w * b.h)[0];
    if (box) this.activateStep(findStep(this.seq.data.steps, box.step), cx);
  }

  sequenceBody(t, file) {
    const seq = this.seq, out = [];
    if (!seq.file && !seq.seed) return [this.note(t, file && !file.checkout ? "The Files selection is not in a checkout this daemon serves." : "Select a file in Files to see its methods and their call sequences.")];
    if (seq.file) {
      const methods = seq.methods || [];
      out.push(v_flex().flex_none().gap(6)
        .child(h_flex().gap(8).items_center()
          .child(div().text_size(12).text_color(t.dim).child("Methods in"))
          .child(div().flex_1().min_w(0).truncate().font_family(settingsMono()).text_size(12).child(seq.file)))
        .when(Boolean(seq.methodsError), (el) => el.child(this.problem(t, seq.methodsError, "Methods")))
        .when(!seq.methods && !seq.methodsError, (el) => el.child(this.note(t, "Reading this file's methods…")))
        .when(Boolean(seq.methods) && !methods.length, (el) => el.child(this.note(t, "No indexed methods in this file.")))
        .child(h_flex().gap(4).flex_wrap().children(methods.filter((m) => !m.name.startsWith("<")).slice(0, 60).map((m) =>
          div().id(`method-${m.id}`).px(6).py(1).rounded(4).cursor_pointer().font_family(settingsMono()).text_size(12)
            .bg(seq.seed?.id === m.id ? t.selected : t.raised).hover((st) => st.bg(t.hover))
            .on_click((_e, cx) => { this.seq.history = []; this.loadSequence(m, false, cx); cx.notify(); })
            .child(m.name)))));
    }
    if (!seq.seed) return out;
    out.push(h_flex().flex_none().gap(8).items_center()
      .when(seq.history.length > 0, (el) => el.child(div().id("sequence-back").px(6).rounded(4).border_1().border_color(t.border).cursor_pointer()
        .hover((st) => st.bg(t.hover)).on_click((_e, cx) => { const back = this.seq.history.pop(); if (back) this.loadSequence(back, false, cx); cx.notify(); })
        .child(`← ${seq.history[seq.history.length - 1]?.name ?? ""}`)))
      .child(div().font_semibold().text_size(14).child(seq.seed.name || seq.seed.id))
      .child(div().flex_1().min_w(0).truncate().text_size(12).text_color(t.dim).child(seq.seed.path || "")));
    if (seq.error) return [...out, this.problem(t, seq.error, "Sequence")];
    if (!seq.data) return [...out, this.note(t, "Reading the static call sequence…")];
    const data = seq.data;
    out.push(div().flex_none().text_size(12).text_color(t.dim).child(`${sequenceHelpers.evidenceSummary(data)} · ${data.hiddenSteps ?? 0} incidental steps hidden${data.truncated ? " · truncated" : ""} · static possible paths, not a runtime trace`));
    const drawn = seq.drawn?.data === data ? seq.drawn : null;
    if (seq.drawError) out.push(this.problem(t, seq.drawError, "Diagram"));
    else if (!drawn) out.push(this.note(t, "Drawing the sequence…"));
    else {
      if (drawn.empty) out.push(this.note(t, drawn.empty));
      if (drawn.asset) out.push(div().id("sequence-diagram").flex_none().w(drawn.width).h(drawn.height).cursor_pointer()
        .on_mouse_down("left", (event, cx) => this.pressSequence(event, cx))
        .child(image(drawn.asset).w(drawn.width).h(drawn.height)));
    }
    out.push(this.stepList(t, data));
    for (const warning of (data.warnings || []).slice(0, 20)) out.push(div().flex_none().text_size(12).text_color(t.ansi3).child(`⚠ ${typeof warning === "string" ? warning : JSON.stringify(warning)}`));
    return out;
  }

  // The steps as rows, in source order: kind, label, line; a proven call target re-centers.
  stepList(t, data) {
    const rows = [], seq = this.seq;
    const walk = (steps, depth, alternate) => {
      for (const step of steps || []) {
        if (rows.length >= MAX_STEP_ROWS) return;
        const target = targetOf(data, step), line = step.range?.startLine;
        rows.push(h_flex().id(`step-${step.id}`).min_h(24).pl(6 + depth * 14).pr(6).gap(8).items_center().rounded(4).cursor_pointer()
          .when(seq.selected === step.id, (el) => el.bg(t.selected)).hover((st) => st.bg(t.hover))
          .on_click((_e, cx) => this.activateStep(step, cx))
          .child(div().w(64).flex_none().text_size(12).text_color(alternate ? t.ansi3 : t.dim).child(alternate ? `else ${step.kind}` : step.kind))
          .child(div().flex_1().min_w(0).truncate().font_family(settingsMono()).text_size(12).child(sequenceHelpers.compactLabel(step)))
          .when(Boolean(target), (el) => el.child(div().flex_none().text_size(12).text_color(t.accent).child(`→ ${target.declaration.name}`)))
          .when(step.kind === "group", (el) => el.child(div().flex_none().text_size(12).text_color(t.dim).child(seq.expanded.has(step.id) ? "collapse" : "expand")))
          .child(div().flex_none().text_size(12).text_color(t.dim).child(line ? `:${line}` : "")));
        walk(step.children, depth + 1, false);
        walk(step.alternate, depth + 1, true);
      }
    };
    walk(data.steps, 0, false);
    return v_flex().flex_none().gap(1)
      .child(div().font_semibold().text_size(14).pb(4).child("Steps"))
      .children(rows)
      .when(rows.length >= MAX_STEP_ROWS, (el) => el.child(this.note(t, `First ${MAX_STEP_ROWS} steps shown.`)));
  }
}

function findStep(steps, id) {
  for (const step of steps || []) {
    if (step.id === id) return step;
    const found = findStep(step.children, id) || findStep(step.alternate, id);
    if (found) return found;
  }
  return null;
}
