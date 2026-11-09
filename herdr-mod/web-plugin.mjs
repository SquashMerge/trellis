// The esbuild plugin for `trellis-web:<name>` imports: see web-scripts.mjs.
import { webModule } from "./web-scripts.mjs";

export const webPlugin = {
  name: "trellis-web",
  setup(b) {
    b.onResolve({ filter: /^trellis-web:/ }, (a) => ({ path: a.path.slice("trellis-web:".length), namespace: "trellis-web" }));
    b.onLoad({ filter: /.*/, namespace: "trellis-web" }, (a) => {
      const { contents, file } = webModule(a.path);
      return { contents, loader: "js", watchFiles: [file] };
    });
  },
};
