// `node --import ./test/register.mjs --test ...`: installs test/hooks.mjs.
import { register } from "node:module";
register("./hooks.mjs", import.meta.url);
