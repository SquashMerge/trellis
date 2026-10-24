// Stable public names keep adapter implementations out of the workbench API.
export { roundTrip as convert, deliver as sendBatch } from "./adapters.js";
export { Pipeline as EventPipeline } from "./pipeline.js";
