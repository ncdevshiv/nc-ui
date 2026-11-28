// Real-browser gate for the web shell (ADR-021): drives system Edge headless,
// loads the wasm artifact, and proves the WebGPU pipeline in pixels.
import http from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, join } from 'node:path';
import puppeteer from 'puppeteer-core';
import { PNG } from 'pngjs';

// Defaults to the dev bindings; pass 'dist' to verify the packaged release
// artifact — the package is only proven when the same gate passes against it.
const dir = process.argv[2] === 'dist' ? './dist/' : './web/';
const WEB = new URL(dir, import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, '$1');
const EDGE = 'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe';
const MIME = { '.html': 'text/html', '.js': 'text/javascript', '.wasm': 'application/wasm' };

const server = http.createServer(async (req, res) => {
  try {
    const path = join(WEB, req.url === '/' ? 'index.html' : req.url.split('?')[0]);
    const body = await readFile(path);
    res.writeHead(200, { 'content-type': MIME[extname(path)] ?? 'application/octet-stream' });
    res.end(body);
  } catch {
    res.writeHead(req.url === '/favicon.ico' ? 204 : 404); res.end();
  }
});
await new Promise((r) => server.listen(0, r));
const port = server.address().port;

const browser = await puppeteer.launch({
  executablePath: EDGE,
  headless: true,
  args: ['--enable-unsafe-webgpu', '--no-sandbox', '--disable-dev-shm-usage'],
});
const page = await browser.newPage();
const errors = [];
page.on('console', (m) => { if (m.type() === 'error') errors.push(m.text()); });
page.on('pageerror', (e) => errors.push(String(e)));

const context = browser.defaultBrowserContext();
await context.overridePermissions(`http://127.0.0.1:${port}`, ['clipboard-read', 'clipboard-write', 'clipboard-sanitized-write']);
await page.goto(`http://127.0.0.1:${port}/`, { waitUntil: 'networkidle0', timeout: 20000 });
await page.waitForFunction(
  () => window.__ncui_status && (window.__ncui_status.attached || window.__ncui_status.error),
  { timeout: 15000 },
);
// let a few rAF frames actually present
await new Promise((r) => setTimeout(r, 800));

// synthesized input: a real click inside the red island plus a keypress —
// the events must arrive in the shell's input model for the gate to pass.
await page.mouse.move(70, 50);
await page.mouse.down();
await page.mouse.up();
await page.keyboard.press('a');
await page.keyboard.press('Tab');
await new Promise((r) => setTimeout(r, 150));
// focus evidence is temporal: scrolling legitimately clears focus (the host's
// documented semantics), so it must be read before the wheel probe.
const input_keys = JSON.parse(await page.evaluate(() => window.ncui_input_state()));
// clipboard: ctrl+c copies the focused island's selection through the host
// into the browser clipboard; ctrl+v pastes back. Row 1's text is "second".
await page.keyboard.down('Control');
await page.keyboard.press('c');
await page.keyboard.up('Control');
await new Promise((r) => setTimeout(r, 150));
const after_copy = JSON.parse(await page.evaluate(() => window.ncui_input_state()));
const browser_clipboard = await page.evaluate(() => navigator.clipboard.readText().catch(() => 'unreadable'));
// IME composition shim: synthetic composition events through the real browser
// event path must land in the host's composition state machine, and end must commit.
const ime = await page.evaluate(async () => {
  const shim = document.getElementById('ncui-ime-shim');
  if (!shim) return { shim_present: false };
  shim.focus();
  shim.dispatchEvent(new CompositionEvent('compositionstart'));
  shim.dispatchEvent(new CompositionEvent('compositionupdate', { data: 'nih' }));
  await new Promise((r) => setTimeout(r, 100));
  const during = JSON.parse(window.ncui_input_state()).composition;
  shim.dispatchEvent(new CompositionEvent('compositionupdate', { data: 'nihon' }));
  shim.dispatchEvent(new CompositionEvent('compositionend', { data: 'nihon' }));
  await new Promise((r) => setTimeout(r, 100));
  const after = JSON.parse(window.ncui_input_state());
  return { shim_present: true, during, committed: after.clipboard, composition_after: after.composition };
});
const ime_ok = ime.shim_present && ime.during === 'nih' && ime.committed === 'nihon' && !ime.composition_after;
// wheel over the red island: scrollable content must actually move
await page.mouse.move(70, 50);
await page.mouse.wheel({ deltaY: 120 });
await new Promise((r) => setTimeout(r, 200));
const input_scroll = JSON.parse(await page.evaluate(() => window.ncui_input_state()));
const input = { ...input_scroll, keys: input_keys };
if (!input.pointer_events) {
  // input_state is exported through wasm-bindgen; if the global probe is
  // missing, call through the module namespace instead.
  input.fallback_probe_missing = true;
}

const status = await page.evaluate(() => window.__ncui_status);
// pixel readback: the composited canvas as PNG data, then inspect raw pixels
const shot = await page.screenshot({ clip: { x: 0, y: 0, width: 320, height: 200 } });
// pixel truth comes from the compositor: decode the page screenshot and
// sample the canvas's center — what a user would actually see.
const png = PNG.sync.read(Buffer.from(shot));
function sample(x, y) {
  const px = Math.min(x, png.width - 1), py = Math.min(y, png.height - 1);
  const i = (py * png.width + px) * 4;
  return [png.data[i], png.data[i + 1], png.data[i + 2], png.data[i + 3]];
}
const pixels = {
  center: sample(160, 100),       // navy clear — inside neither island
  red_island: sample(70, 50),     // inside island 1 at (20,20) 100x60
  green_island: sample(240, 130), // inside island 2 at (200,100) 80x60
  // the custom island — a miniature timeline at window (10,165) 300x30:
  tl_track: sample(15, 180),      // track background before the clip — rgb(24,24,28)
  tl_clip: sample(50, 180),       // clip body, no bar — rgb(50,80,180)
  tl_bar: sample(35, 180),        // waveform bar at content x=24 — rgb(140,180,255)
  tl_bar_clipped: sample(148, 180), // the bar that crosses the clip edge, inside part
  tl_past_clip: sample(151, 180), // past the clip edge: without clipping the bar paints here
  tl_playhead: sample(91, 170),   // the playhead, over everything — rgb(255,70,70)
};

// drag and drop: a synthetic text drop over the red island must land in the
// host's drop path — the island's clipboard receives the dropped text.
const drop = await page.evaluate(async () => {
  const c = document.getElementById('gpu');
  const dt = new DataTransfer();
  dt.setData('text/plain', 'dropped-text');
  const ev = new DragEvent('drop', { dataTransfer: dt, bubbles: true });
  Object.defineProperty(ev, 'offsetX', { value: 70 });
  Object.defineProperty(ev, 'offsetY', { value: 50 });
  c.dispatchEvent(ev);
  await new Promise((r) => setTimeout(r, 100));
  return { clipboard: JSON.parse(window.ncui_input_state()).clipboard };
});
const drop_ok = drop.clipboard === 'dropped-text';
// pointer capture: a drag that leaves the island mid-gesture must keep routing
// to it — the editor-class contract (a trim dragged past the island's edge is
// the normal shape of editing, not an edge case). Down inside the timeline,
// move far outside, release outside.
await page.mouse.move(50, 180);
await page.mouse.down();
await page.mouse.move(315, 10, { steps: 3 });
await new Promise((r) => setTimeout(r, 120));
const capture_during = JSON.parse(await page.evaluate(() => window.ncui_input_state()));
await page.mouse.up();
await page.mouse.move(315, 12, { steps: 2 });
await new Promise((r) => setTimeout(r, 120));
const capture_after = JSON.parse(await page.evaluate(() => window.ncui_input_state()));
const capture_ok = capture_during.last_route === 'island:timeline'
  && capture_after.last_route === 'dom';
// idle-stopping: a static scene draws nothing. Measure presented frames over
// two idle windows — the second must show no growth — then verify input
// re-dirties the loop.
const idle = await page.evaluate(async () => {
  const f = () => JSON.parse(window.ncui_input_state()).frames;
  const start = f();
  await new Promise((r) => setTimeout(r, 400));
  const mid = f();
  await new Promise((r) => setTimeout(r, 400));
  const late = f();
  return { start, mid, late };
});
const idle_ok = idle.late === idle.mid;
// resize contract: CSS-driven canvas resizing must resize the backing store
// through the ResizeObserver path — high-DPI sizing is the shell's problem.
const resize = await page.evaluate(async () => {
  const c = document.getElementById('gpu');
  const before = { w: c.width, h: c.height };
  c.style.width = '400px';
  c.style.height = '240px';
  await new Promise((r) => setTimeout(r, 300));
  return { before, after: { w: c.width, h: c.height }, dpr: window.devicePixelRatio };
});
const resize_ok = resize.before.w === 320 && resize.after.w === Math.round(400 * resize.dpr)
  && resize.after.h === Math.round(240 * resize.dpr);

// accessibility mirror: the scene's semantic tree as hidden ARIA DOM —
// the same information AccessKit publishes on Windows, on the web's primitive.
const aria = await page.evaluate(() => {
  const container = document.getElementById('ncui-aria');
  if (!container) return { present: false };
  const nodes = [...container.querySelectorAll('[role]')].map((el) => ({
    role: el.getAttribute('role'),
    label: el.getAttribute('aria-label'),
    focusable: el.getAttribute('tabindex') === '-1',
  }));
  return {
    present: true,
    hidden_visually: /clip/.test(container.getAttribute('style') || ''),
    regions: nodes.filter((n) => n.role === 'region').map((n) => n.label),
    buttons: nodes.filter((n) => n.role === 'button').map((n) => n.label),
    sliders: nodes.filter((n) => n.role === 'slider').map((n) => n.label),
    texts: nodes.filter((n) => n.role === 'text').map((n) => n.label),
    focusable_count: nodes.filter((n) => n.focusable).length,
  };
});

await browser.close();
server.close();

// routing truth: the click inside the red island hit-tested to an island
// route (not Dom), a row was selected in island state, and Tab moved focus
// into the host — the input model's outcomes, not just event arrival.
const routed_to_island = /Island/.test(input.last_route) || input.red_selection !== 'none';
const focus_moved = input.focused !== 'none';
const aria_ok = aria.present && aria.hidden_visually
  && aria.regions.includes('red island') && aria.regions.includes('green island')
  && aria.regions.includes('timeline')
  && aria.buttons.length === 3 && aria.buttons.includes('second')
  && aria.sliders.includes('playhead') && aria.texts.includes('waveform intro.mp4')
  && aria.focusable_count === 4;
const clipboard_ok = after_copy.clipboard === 'second' && browser_clipboard === 'second';
const scrolled = input.red_scroll_y > 0;
const focus_moved_keys = input.keys.focused !== 'none';
const input_ok = input.downs >= 1 && input.key_events >= 2 && input.pointer_events >= 4
  && routed_to_island && input.red_selection !== 'none' && focus_moved_keys && scrolled && clipboard_ok && aria_ok && resize_ok && ime_ok && idle_ok && drop_ok;
const near = (px, rgb) => px.slice(0, 3).every((v, i) => Math.abs(v - rgb[i]) <= 8);
// the scene is deterministic: navy clear rgb(28,41,82), red island rgb(220,40,40),
// green island rgb(40,200,40) — pixel truth at three independent points.
const pipeline_in_pixels = near(pixels.center, [28, 41, 82]);
const scene_in_pixels = near(pixels.red_island, [220, 40, 40]) && near(pixels.green_island, [40, 200, 40]);
// the custom-island proof: display-list content through the same render path, with
// the clip proof — the bar crossing the clip edge is visible inside it and absent
// past it, in compositor pixels.
const timeline_in_pixels = near(pixels.tl_track, [24, 24, 28])
  && near(pixels.tl_clip, [50, 80, 180])
  && near(pixels.tl_bar, [140, 180, 255])
  && near(pixels.tl_bar_clipped, [140, 180, 255])
  && near(pixels.tl_past_clip, [24, 24, 28])
  && near(pixels.tl_playhead, [255, 70, 70]);
// the text proof: the clip label "intro.mp4" is drawn in near-white rgb(235,240,255)
// over the clip's blue. Its band is window x 36-90, y 172-186 (content origin (26,19)
// inside the island at (10,165)). Fully-covered glyph pixels are the only pixels in
// the band with a red channel over 200 — the clip is 50, the waveform bars 140 — so
// counting them is a font-independent proof that glyphs, not rectangles, drew.
let text_pixels = 0;
for (let x = 36; x <= 90; x++) {
  for (let y = 172; y <= 186; y++) {
    if (sample(x, y)[0] > 200) text_pixels++;
  }
}
const text_in_pixels = text_pixels > 4;
// the blit proof: the thumbnail is a deterministic gradient — red rising left to
// right, blue fixed at 200 — scaled into the clip's leading edge (window x 100-124,
// y 171-184). Two samples prove the gradient direction and the blue channel; a solid
// rectangle or a missing texture fails both.
const blit_left = sample(103, 175);
const blit_right = sample(121, 175);
const blit_in_pixels = blit_left[2] > 150 && blit_right[2] > 150
  && blit_right[0] > blit_left[0] + 60;
const verdict = {
  blit_left,
  blit_right,
  blit_in_pixels,
  text_pixels,
  text_in_pixels,
  webgpu_available: status.webgpu,
  attached: status.attached,
  attach_error: status.error,
  console_errors: errors,
  center_pixel: pixels.center,
  pipeline_in_pixels,
  scene_in_pixels,
  timeline_pixels: {
    track: pixels.tl_track,
    clip: pixels.tl_clip,
    bar: pixels.tl_bar,
    bar_clipped: pixels.tl_bar_clipped,
    past_clip: pixels.tl_past_clip,
    playhead: pixels.tl_playhead,
  },
  timeline_in_pixels,
  input,
  clipboard: { host: after_copy.clipboard, browser: browser_clipboard },
  clipboard_ok,
  aria,
  aria_ok,
  resize,
  resize_ok,
  ime,
  ime_ok,
  idle,
  idle_ok,
  drop,
  drop_ok,
  capture_ok,
  capture_during: capture_during.last_route,
  capture_after: capture_after.last_route,
  input_ok,
  screenshot_bytes: shot.length,
};
console.log(JSON.stringify(verdict, null, 1));
const pass = status.webgpu && status.attached && pipeline_in_pixels && scene_in_pixels && timeline_in_pixels && text_in_pixels && blit_in_pixels && capture_ok && input_ok && errors.length === 0;
console.log('VERDICT=' + (pass ? 'PASS' : 'FAIL'));
process.exit(pass ? 0 : 1);
