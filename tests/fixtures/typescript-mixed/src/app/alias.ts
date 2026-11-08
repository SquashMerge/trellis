import { Repo } from "@app/repo";
import { Model } from "app/model";
import m = require("./model");

export const repo = new Repo<Model>();
export const viaRequire = m;
