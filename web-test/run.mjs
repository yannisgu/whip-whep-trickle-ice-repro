// Runs the test page in Chromium and Firefox and prints what happened.
//
//   node run.mjs [http://127.0.0.1:8189] [chromium,firefox]
//
// Start the server first: cargo run --features upstream|fork --bin web

import { chromium, firefox, webkit } from "playwright";

const base = process.argv[2] ?? "http://127.0.0.1:8189";
const browsers = (process.argv[3] ?? "chromium,firefox").split(",");
const engines = { chromium, firefox, webkit };

const scenarios = [
  { proto: "whip", trickle: 1, nobundle: 0 },
  { proto: "whip", trickle: 1, nobundle: 1 },
  { proto: "whep", trickle: 1, nobundle: 0 },
  { proto: "whep", trickle: 1, nobundle: 1 },
];

const launchOptions = {
  chromium: { args: ["--autoplay-policy=no-user-gesture-required"] },
  firefox: { firefoxUserPrefs: { "media.autoplay.default": 0 } },
  webkit: {},
};

for (const name of browsers) {
  const browser = await engines[name].launch(launchOptions[name]);
  for (const s of scenarios) {
    const page = await browser.newPage();
    const url = `${base}/?proto=${s.proto}&trickle=${s.trickle}&nobundle=${s.nobundle}&run=1`;
    await page.goto(url);
    await page.waitForFunction(() => window.reproResult?.done, null, { timeout: 30000 });
    const r = await page.evaluate(() => window.reproResult);
    await page.click("#stop");
    await page.waitForTimeout(500);
    await page.close();
    report(name, r);
  }
  await browser.close();
}

function report(browser, r) {
  const trickled = r.patches.filter((p) => !p.candidate.includes("end-of-candidates"));
  const statuses = {};
  for (const p of trickled) statuses[p.status] = (statuses[p.status] ?? 0) + 1;
  const perMline = {};
  for (const t of r.server) perMline[t.mline] = (perMline[t.mline] ?? 0) + 1;
  const sentPerMid = {};
  for (const p of trickled) sentPerMid[p.mid] = (sentPerMid[p.mid] ?? 0) + 1;
  const wrong = r.server.filter((t) => t.expected != null && t.expected != t.mline).length;
  console.log(
    `${browser.padEnd(8)} ${r.proto.toUpperCase()} ${r.nobundle ? "no BUNDLE" : "BUNDLE   "}` +
      ` | ${r.plugin}` +
      `\n    mids ${r.mids.join(",")}, offer candidates ${r.offerCandidates}` +
      `\n    trickled per mid ${JSON.stringify(sentPerMid)}, PATCH statuses ${JSON.stringify(statuses)}` +
      `\n    handed to webrtcbin per m-line ${JSON.stringify(perMline)}, on the wrong m-line: ${wrong}` +
      `\n    ICE connected after ${r.connectedMs ?? "-"} ms${r.error ? `, error: ${r.error}` : ""}`,
  );
}
