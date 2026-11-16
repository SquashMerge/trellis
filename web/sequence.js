"use strict";
// The Sequence panel's two plain DOM states: empty and could-not-draw. The model, the display projection and
// the per-container render owner are sequence-model.js, which loads first and publishes window.TrellisSequence.
// Replaces a diagram with its empty state. `onShow`, when given, is a real control.
function renderSequenceEmpty(container, summary, onShow, preserveDiagram = false) {
  const note = document.createElement("p");
  note.className = "sequence-empty"; note.textContent = `${summary} `;
  if (onShow) {
    const show = document.createElement("button");
    show.type = "button"; show.textContent = "Show all steps";
    show.addEventListener("click", onShow);
    note.append(show);
  }
  if (preserveDiagram) container.prepend(note); else container.replaceChildren(note);
}
// Replaces a diagram that could not be drawn with the renderer's reason (#1007). Nothing else
// is drawn in its place. It takes the container's render ownership, as a drawing would, so an
// older render cannot install over it and the controls of a replaced diagram stay inert.
function renderSequenceError(container, reason) {
  beginSequenceRender(container);
  const note = document.createElement("p");
  note.className = "sequence-empty"; note.textContent = `This Sequence could not be drawn: ${reason}`;
  container.replaceChildren(note);
}

Object.assign(window.TrellisSequence, {renderEmpty:renderSequenceEmpty, renderError:renderSequenceError});
