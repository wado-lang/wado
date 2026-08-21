// wasi:http/types for a Worker, from `preview3-shim`'s Node implementation.
// By path, because the package's `exports` map serves a Worker its browser
// build, whose every method throws `Todo`. See README.

import { Fields } from "../node_modules/@bytecodealliance/preview3-shim/dist/nodejs/http/fields.js";
import {
  Request,
  RequestOptions,
} from "../node_modules/@bytecodealliance/preview3-shim/dist/nodejs/http/request.js";
import { Response } from "../node_modules/@bytecodealliance/preview3-shim/dist/nodejs/http/response.js";

export const types = { Fields, Request, RequestOptions, Response };
