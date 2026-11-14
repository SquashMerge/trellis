"use strict";
// The app's classic scripts in the order web/index.html loads them. app.js is last: it holds every
// statement that runs at load, and the scripts before it only declare. The UI harnesses load this list.
exports.APP_SCRIPTS = ["app-state.js", "app-api.js", "app-agent.js", "app-navigation.js", "app-checkout.js", "app-source.js",
  "app-files.js", "app-tree.js", "app-sequence.js", "app-libraries.js", "app-views.js", "app.js"];
// What each script needs from an earlier script while it loads. The app-*.js scripts need nothing from
// each other then (they only declare); app-state.js reads two published globals in its initialisers.
exports.LOAD_TIME_NEEDS = {
  "app-state.js": ["index-pin.js", "architecture-apps.js"],
  "app.js": ["shell.js", "classes.js", "navigation.js", "viewer-metrics.js", "treemap.js", "architecture.js", ...exports.APP_SCRIPTS.slice(0, -1)],
};
