// Transpile a Wado component to JS with the released jco.
//
// Usage: node transpile-released.mjs <component.wasm> [output-dir]
//
// Run the result on Node 26+ (stable JSPI, no flag needed):
//   node -e "import('<dir>/<name>.js').then(m => m.run.run())"

import { transpile } from "@bytecodealliance/jco";
import { copyFile, readdir, readFile, writeFile, mkdir, symlink, rm } from "node:fs/promises";
import { basename, dirname, join } from "node:path";

const wasmPath = process.argv[2];
if (!wasmPath) {
  console.error("Usage: node transpile-released.mjs <component.wasm> [output-dir]");
  process.exit(1);
}
const here = dirname(new URL(import.meta.url).pathname);
const name = basename(wasmPath).replace(/\.wasm$/, "");
const outDir = process.argv[3] ?? wasmPath.replace(/\.wasm$/, "-jco-released");
const component = await readFile(wasmPath);

// The module a Wado component's main module takes its web imports from: the
// functions whose signature holds a reference, which the Component Model
// cannot carry. The component fills it with functions that trap.
const WEB_IMPORTS = "extern";

async function write(files) {
  for (const [file, bytes] of Object.entries(files)) {
    const p = join(outDir, file);
    await mkdir(dirname(p), { recursive: true });
    await writeFile(p, bytes);
  }
}

const isCoreModule = (file) => /\.core\d*\.wasm$/.test(file);
const importsWeb = (bytes) =>
  WebAssembly.Module.imports(new WebAssembly.Module(bytes)).some((i) => i.module === WEB_IMPORTS);

// jco's own WASI shim (`preview3-shim`) serves the `wasi:*` imports.
let { files } = await transpile(component, { name });
const web = Object.entries(files).some(([file, bytes]) => isCoreModule(file) && importsWeb(bytes));
if (web) {
  // jco instantiates each core module itself, with no way in. Its
  // `instantiation` mode hands each one's imports to a function of ours,
  // which is where `package-web`'s glue takes the place of the traps.
  const loaded = await transpile(component, {
    name: `${name}.component`,
    instantiation: { tag: "async" },
  });
  files = loaded.files;
  files[`${name}.js`] = new TextEncoder().encode(await loader(loaded));
}
await write(files);

// The module `<name>.js` of a component with web imports: it instantiates the
// component over the glue and exports what the component does.
async function loader({ imports, exports }) {
  const glueDir = join(here, "../../package-web/glue");
  const glue = (await readdir(glueDir)).filter((file) => file.endsWith(".js"));
  // Every glue module answers `$class-of` and its siblings from its own class
  // table, and the loader merges their imports under those names.
  if (glue.length !== 1) {
    throw new Error(`the loader links one glue module, and ${glueDir} holds ${glue.length}`);
  }
  const camel = (kebab) => kebab.replace(/-(\w)/g, (_, c) => c.toUpperCase());
  const lines = [`import { instantiate } from "./${name}.component.js";`];
  await mkdir(outDir, { recursive: true });
  for (const [i, file] of glue.entries()) {
    await copyFile(join(glueDir, file), join(outDir, `web-${file}`));
    lines.push(`import { $glue as $glue${i} } from "./web-${file}";`);
  }
  // `wasi:<package>/<interface>` is the shim's module `<package>`, which
  // exports each interface in lower camel case.
  const wasi = imports.map((specifier, i) => {
    const [, pkg, iface] = specifier.match(/^wasi:([\w-]+)\/([\w-]+)/) ?? [];
    if (pkg === undefined) {
      throw new Error(`no host serves the import \`${specifier}\``);
    }
    lines.push(`import { ${camel(iface)} as $wasi${i} } from "@bytecodealliance/preview3-shim/${pkg}";`);
    return `  ${JSON.stringify(specifier)}: $wasi${i},`;
  });
  lines.push(
    "",
    `const $glues = [${glue.map((_, i) => `$glue${i}()`).join(", ")}];`,
    "const $web = Object.assign({}, ...$glues.map((glue) => glue.imports));",
    "",
    "// The main module is the one that takes web imports. The glue reads its",
    "// memory and calls its exports, so it is handed both once the module exists.",
    "async function $instantiateCore(module, imports) {",
    `  if (imports?.${WEB_IMPORTS} === undefined) {`,
    "    return WebAssembly.instantiate(module, imports);",
    "  }",
    "  const instance = await WebAssembly.instantiate(module, {",
    "    ...imports,",
    `    ${WEB_IMPORTS}: { ...imports.${WEB_IMPORTS}, ...$web },`,
    "  });",
    "  const core = {",
    "    memory: imports.mem.memory,",
    "    realloc: imports.mem.realloc,",
    "    exports: instance.exports,",
    "  };",
    "  for (const glue of $glues) {",
    "    glue.connect(core);",
    "  }",
    "  return instance;",
    "}",
    "",
    "const $root = await instantiate(undefined, {",
    ...wasi,
    "}, $instantiateCore);",
    "",
  );
  for (const [i, [exported]] of exports.entries()) {
    lines.push(
      `const $export${i} = $root[${JSON.stringify(exported)}];`,
      `export { $export${i} as ${JSON.stringify(exported)} };`,
    );
  }
  return `${lines.join("\n")}\n`;
}

await writeFile(join(outDir, "package.json"), '{"type":"module"}\n');
// The output imports the shim by bare specifier, and `outDir` is usually a temp
// directory with no `node_modules` above it.
const deps = join(here, "node_modules");
await rm(join(outDir, "node_modules"), { force: true, recursive: true });
await symlink(deps, join(outDir, "node_modules"), "dir");
console.error(`transpiled (released jco) → ${outDir}`);
