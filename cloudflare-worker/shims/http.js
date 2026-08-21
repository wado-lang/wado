// wasi:http/types for a Worker, from `preview3-shim`'s Node implementation.
//
// Reached by path rather than by package name: the package's `exports` map
// serves a Worker its browser build, and every method of that build throws
// `Todo`. Naming the three modules also leaves out `client.js` and `server.js`,
// which import `node:worker_threads`.

import { Fields } from "../node_modules/@bytecodealliance/preview3-shim/dist/nodejs/http/fields.js";
import {
  Request,
  RequestOptions,
} from "../node_modules/@bytecodealliance/preview3-shim/dist/nodejs/http/request.js";
import { Response } from "../node_modules/@bytecodealliance/preview3-shim/dist/nodejs/http/response.js";

export const types = { Fields, Request, RequestOptions, Response };
