// Agent shows (#818): a status poll's `viewRequest` takes an unpinned pane to that Sequence, with a
// note and Back. These are methods of the pane (main.js installs them on its class unchanged).
import { div } from "gpui-kit";
import { h_flex } from "gpui-base";
import { pinLabel, pinOf, samePin } from "./client.js";

export class AgentShow {
  // --- agent shows (#818) ---

  // The first request seen for a checkout is a baseline; a newer generation is a show, which a
  // pinned pane counts as seen and ignores. Only this pane's display changes, never focus.
  observeShow(value, cx) {
    const request = value == null ? null : viewRequestOf(value);
    if (value != null && !request) return;
    const generation = request?.generation ?? 0;
    if (!this.showSeen || this.showSeen.rootKey !== this.rootKey) { this.showSeen = { rootKey: this.rootKey, generation }; return; }
    if (generation <= this.showSeen.generation) return;
    this.showSeen.generation = generation;
    if (this.pinned) return;
    this.showRequest(request, cx);
  }

  showRequest(request, cx) {
    const back = this.shown?.back ?? null;
    const label = request.view === "source" ? `${request.path}:${request.line}` : request.view === "class" ? "Class diagram" : "Sequence";
    // The deep link's stale-pin rule: a target from another snapshot is not shown.
    if (!samePin(request.revision, this.status?.pin)) {
      this.shown = { text: `The agent showed a ${label} at index ${pinLabel(request.revision)}; this pane is at ${this.status?.pin ? pinLabel(this.status.pin) : "no index"}.`, back };
      return;
    }
    if (request.view !== "sequence") {
      this.shown = { text: `The agent showed ${label}. This pane has no ${request.view === "class" ? "Class" : "source"} view; open it in the browser.`, back };
      return;
    }
    const before = back ?? { tab: this.tab, seq: this.seq };
    const prev = this.seq;
    this.tab = "sequence";
    // The Files selection's method list stays; only the shown method changes.
    this.seq = { file: prev.file, methods: prev.methods, methodsError: prev.methodsError, history: [], expanded: new Set(), agent: true };
    this.loadSequence({ id: request.symbolId, name: null, path: null }, false, cx);
    this.shown = { text: "Shown by agent · Sequence", back: before };
  }

  // Back returns to the tab and sequence shown before the agent's first show.
  backFromShow(cx) {
    const back = this.shown?.back;
    this.shown = null;
    if (back) {
      this.tab = back.tab; this.seq = back.seq;
      // The earlier drawing's file was replaced, so it is drawn again.
      if (this.seq) this.seq.drawn = null;
      this.seqTried = null;
      // Never an older revision's payload under this header: read it again at this pin.
      const data = this.seq?.data;
      if (this.seq?.seed && !this.seq.loading && (!data || !samePin(data.revision, this.status?.pin))) this.reloadSequence(cx);
    }
    cx.notify();
  }

  shownBar(t) {
    if (!this.shown) return [];
    return [h_flex().id("agent-shown").flex_none().gap(8).items_center().px(8).py(4).rounded(6).bg(t.surface).border_1().border_color(t.border)
      .child(div().flex_1().min_w(0).whitespace_normal().text_size(12).text_color(t.muted).child(this.shown.text))
      .when(Boolean(this.shown.back), (el) => el.child(div().id("agent-shown-back").px(6).rounded(4).border_1().border_color(t.border).cursor_pointer()
        .hover((st) => st.bg(t.hover)).on_click((_e, cx) => this.backFromShow(cx)).child("Back")))
      .child(div().id("agent-shown-close").text_color(t.dim).cursor_pointer().child("×").on_click((_e, cx) => { this.shown = null; cx.notify(); }))];
  }
}

// A status poll's `viewRequest` (#818), or null when it isn't one this pane understands.
function viewRequestOf(value) {
  if (!value || typeof value !== "object" || !Number.isSafeInteger(value.generation) || value.generation < 1) return null;
  let revision;
  try { revision = pinOf(value.revision); } catch { return null; }
  if ((value.view === "sequence" || value.view === "class") && typeof value.symbolId === "string" && value.symbolId)
    return { generation: value.generation, view: value.view, symbolId: value.symbolId, revision };
  if (value.view === "source" && typeof value.path === "string" && value.path && Number.isSafeInteger(value.line) && value.line >= 1)
    return { generation: value.generation, view: "source", path: value.path, line: value.line, revision };
  return null;
}
