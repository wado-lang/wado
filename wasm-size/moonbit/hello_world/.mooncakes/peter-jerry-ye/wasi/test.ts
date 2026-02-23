import { assertEquals } from "@std/assert";
import { assertSnapshot } from "@std/testing/snapshot";
import { assert } from "@std/assert/assert";

const textDecoder = new TextDecoder("utf-8");
const textEncoder = new TextEncoder();

Deno.test("core wasi", async (t) => {
  await t.step("arg_get", async (t) => {
    const output = await new Deno.Command("wasmtime", {
      args: [
        "--argv0",
        "args_get.wasm",
        "test/target/wasm/debug/build/args_get/args_get.wasm",
      ],
      stdout: "piped",
      stderr: "piped",
    }).spawn().output();
    assertEquals(output.code, 0);
    const stdout = textDecoder.decode(output.stdout);
    await assertSnapshot(t, stdout);
  });

  await t.step("environ_get", async (t) => {
    const output = await new Deno.Command("wasmtime", {
      args: [
        "--env",
        "hello=world",
        "test/target/wasm/debug/build/environ_get/environ_get.wasm",
      ],
      stdout: "piped",
      stderr: "piped",
    }).spawn().output();
    assertEquals(output.code, 0);
    const stdout = textDecoder.decode(output.stdout);
    await assertSnapshot(t, stdout);
  });

  await t.step("fd_fdstat_get", async (t) => {
    const output = await new Deno.Command("wasmtime", {
      args: [
        "--dir",
        "test::.",
        "test/target/wasm/debug/build/fd_fdstat_get/fd_fdstat_get.wasm",
      ],
      stdout: "piped",
      stderr: "piped",
    }).spawn().output();
    assertEquals(output.code, 0);
    const stdout = textDecoder.decode(output.stdout);
    await assertSnapshot(t, stdout);
  });

  await t.step("fd_prestat_dir_name", async (t) => {
    const output = await new Deno.Command("wasmtime", {
      args: [
        "--dir",
        ".",
        "--dir",
        "test",
        "test/target/wasm/debug/build/fd_prestat_dir_name/fd_prestat_dir_name.wasm",
      ],
      stdout: "piped",
      stderr: "piped",
    }).spawn().output();
    assertEquals(output.code, 0);
    const stdout = textDecoder.decode(output.stdout);
    await assertSnapshot(t, stdout);
  });

  await t.step("fd_read", async (t) => {
    // Notice that wasmtime may only read into the first buffer.
    let process = new Deno.Command("wasmtime", {
      args: ["test/target/wasm/debug/build/fd_read/fd_read.wasm"],
      stdin: "piped",
      stdout: "piped",
      stderr: "piped",
    }).spawn();
    let writer = process.stdin.getWriter();
    await writer.write(textEncoder.encode("hello"));
    await writer.close();
    let output = await process.output();
    assertEquals(output.code, 0);
    let stdout = textDecoder.decode(output.stdout);
    await assertSnapshot(t, stdout);

    process = new Deno.Command("wasmtime", {
      args: ["test/target/wasm/debug/build/fd_read/fd_read.wasm"],
      stdin: "piped",
      stdout: "piped",
      stderr: "piped",
    }).spawn();
    writer = process.stdin.getWriter();
    await writer.write(
      textEncoder.encode(
        "hello, this is a longer sentence, as a quick brown fox jumps over another buffer",
      ),
    );
    await writer.close();
    output = await process.output();
    assertEquals(output.code, 0);
    stdout = textDecoder.decode(output.stdout);
    await assertSnapshot(t, stdout);
  });

  await t.step("fd_readdir", async (t) => {
    const output = await new Deno.Command("wasmtime", {
      args: [
        "--dir",
        "test/fd_readdir",
        "test/target/wasm/debug/build/fd_readdir/fd_readdir.wasm",
      ],
      stdout: "piped",
      stderr: "piped",
    }).spawn().output();
    assertEquals(output.code, 0);
    const stdout = textDecoder.decode(output.stdout);
    await assertSnapshot(t, stdout);
  });

  await t.step("fd_write", async (t) => {
    // Notice that wasmtime may only write from the first buffer.
    const output = await new Deno.Command("wasmtime", {
      args: ["test/target/wasm/debug/build/fd_write/fd_write.wasm"],
      stdout: "piped",
      stderr: "piped",
    }).spawn().output();
    assertEquals(output.code, 0);
    const stdout = textDecoder.decode(output.stdout);
    await assertSnapshot(t, stdout);
  });

  await t.step("poll_oneoff", async () => {
    const start = performance.now();
    await new Deno.Command("wasmtime", {
      args: ["test/target/wasm/debug/build/poll_oneoff/poll_oneoff.wasm"],
    }).output();
    const end = performance.now();
    assert(
      end - start > 1,
      `only toke ${end - start}ms, expected more than 1ms`,
    );
  });

  await t.step("proc_exit", async () => {
    const output = await new Deno.Command("wasmtime", {
      args: ["test/target/wasm/debug/build/proc_exit/proc_exit.wasm"],
      stdout: "piped",
      stderr: "piped",
    }).spawn().output();
    assertEquals(output.code, 122);
  });
});

Deno.test("extensions", async (t) => {
  await t.step("fs_exists", async (t) => {
    const output = await new Deno.Command("wasmtime", {
      args: [
        "--dir",
        "test::.",
        "test/target/wasm/debug/build/fs_exists/fs_exists.wasm",
      ],
      stdout: "piped",
      stderr: "piped",
    }).spawn().output();
    assertEquals(output.code, 0);
    const stdout = textDecoder.decode(output.stdout);
    await assertSnapshot(t, stdout);
  });

  await t.step("fs_can_read", async (t) => {
    const output = await new Deno.Command("wasmtime", {
      args: [
        "--dir",
        "test::.",
        "test/target/wasm/debug/build/fs_can_read/fs_can_read.wasm",
      ],
      stdout: "piped",
      stderr: "piped",
    }).spawn().output();
    assertEquals(output.code, 0);
    const stdout = textDecoder.decode(output.stdout);
    await assertSnapshot(t, stdout);
  });

  await t.step("fs_can_write", async (t) => {
    const output = await new Deno.Command("wasmtime", {
      args: [
        "--dir",
        "test::.",
        "test/target/wasm/debug/build/fs_can_write/fs_can_write.wasm",
      ],
      stdout: "piped",
      stderr: "piped",
    }).spawn().output();
    assertEquals(output.code, 0);
    const stdout = textDecoder.decode(output.stdout);
    await assertSnapshot(t, stdout);
  });

  await t.step("fs_can_execute", async (t) => {
    const output = await new Deno.Command("wasmtime", {
      args: [
        "--dir",
        "test::.",
        "test/target/wasm/debug/build/fs_can_execute/fs_can_execute.wasm",
      ],
      stdout: "piped",
      stderr: "piped",
    }).spawn().output();
    assertEquals(output.code, 0);
    const stdout = textDecoder.decode(output.stdout);
    await assertSnapshot(t, stdout);
  });

  await t.step("fs_atime", async (t) => {
    const output = await new Deno.Command("wasmtime", {
      args: [
        "--dir",
        "test::.",
        "test/target/wasm/debug/build/fs_atime/fs_atime.wasm",
      ],
      stdout: "piped",
      stderr: "piped",
    }).spawn().output();
    assertEquals(output.code, 0);
    const stdout = textDecoder.decode(output.stdout);
    await assertSnapshot(t, stdout);
  });

  await t.step("fs_kind", async (t) => {
    const output = await new Deno.Command("wasmtime", {
      args: [
        "--dir",
        "test::.",
        "test/target/wasm/debug/build/fs_kind/fs_kind.wasm",
      ],
      stdout: "piped",
      stderr: "piped",
    }).spawn().output();
    assertEquals(output.code, 0);
    const stdout = textDecoder.decode(output.stdout);
    await assertSnapshot(t, stdout);
  });

  await t.step("fs_readdir", async (t) => {
    const output = await new Deno.Command("wasmtime", {
      args: [
        "--dir",
        "test::.",
        "test/target/wasm/debug/build/fs_readdir/fs_readdir.wasm",
      ],
      stdout: "piped",
      stderr: "piped",
    }).spawn().output();
    assertEquals(output.code, 0);
    const stdout = textDecoder.decode(output.stdout);
    await assertSnapshot(t, stdout);
  });

  await t.step("fs_read_text", async (t) => {
    const output = await new Deno.Command("wasmtime", {
      args: [
        "--dir",
        "test::.",
        "test/target/wasm/debug/build/fs_read_text/fs_read_text.wasm",
      ],
      stdout: "piped",
      stderr: "piped",
    }).spawn().output();
    assertEquals(output.code, 0);
    const stdout = textDecoder.decode(output.stdout);
    await assertSnapshot(t, stdout);
  });

  await t.step("fs_walk", async (t) => {
    const output = await new Deno.Command("wasmtime", {
      args: [
        "--dir",
        "test::.",
        "test/target/wasm/debug/build/fs_walk/fs_walk.wasm",
      ],
      stdout: "piped",
      stderr: "piped",
    }).spawn().output();
    assertEquals(output.code, 0);
    const stdout = textDecoder.decode(output.stdout);
    await assertSnapshot(t, stdout);
  });
});
