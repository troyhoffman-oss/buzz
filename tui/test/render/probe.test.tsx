import { test, expect } from "bun:test";
import { testRender } from "@opentui/solid";
import pal from "../../themes/probe.json";

test("spans + json", async () => {
  const t = await testRender(
    () => (
      <box style={{ flexDirection: "column", width: "100%", height: "100%" }}>
        <text>
          <span style={{ fg: pal.a }}>ab</span>
          <span style={{ fg: pal.b, bold: true }}>cd</span>
        </text>
        <text>{"line2 ─ ⬤ ✎"}</text>
      </box>
    ),
    { width: 20, height: 4 },
  );
  await t.renderOnce();
  console.log(JSON.stringify(t.captureCharFrame()));
});
