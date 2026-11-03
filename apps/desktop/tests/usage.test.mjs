import { test } from "node:test";
import assert from "node:assert/strict";
import { contextPercent, sessionUsage } from "../src/lib/usage.ts";

const run = (usage) => ({ kind: "run_end", usage });

test("session usage sums finished runs and ignores other items", () => {
  assert.equal(sessionUsage([]), null);
  assert.equal(sessionUsage([{ kind: "user" }, run(null)]), null);
  assert.deepEqual(
    sessionUsage([
      run({
        input_tokens: 100,
        cache_read_input_tokens: 40,
        cache_write_input_tokens: 10,
        output_tokens: 20,
      }),
      { kind: "assistant" },
      run({
        input_tokens: 50,
        cache_read_input_tokens: 0,
        cache_write_input_tokens: 5,
        output_tokens: 30,
        reasoning_tokens: 12,
      }),
    ]),
    {
      input_tokens: 150,
      cache_read_input_tokens: 40,
      cache_write_input_tokens: 15,
      output_tokens: 50,
      reasoning_tokens: 12,
    },
  );
});

test("context percent is whole and capped", () => {
  assert.equal(contextPercent(378_800, 1_000_000), 38);
  assert.equal(contextPercent(2_000, 1_000), 100);
  assert.equal(contextPercent(10, 0), 0);
});
