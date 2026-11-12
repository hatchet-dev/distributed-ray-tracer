// The live preview: the script's parts as meshes in three.js, with the render camera as an object
// in the scene. Drag the background to orbit the view, shift-drag to pan, scroll to zoom; arrow
// keys walk the view around (shift+arrows turn and tilt it). On touch screens, pinch to zoom.
//
// - Click a labelled part and drag its arrows to move it (W), or its rings to turn it (E): on
//   release, the `[x, y, z]` its `at` or `rotate` is written as (found by crates/preview) is
//   rewritten in the script, or a `rotate` is added after its `at`. Parts that share their `at`
//   text move and turn together.
// - Click the camera to move it (W) or turn it (E): the next render is taken from there. It turns
//   left/right and up/down only (the renderer's camera is always level).
// - Camera view (C) looks through the camera to see exactly what the render frames; then dragging and scrolling
//   orbit and zoom the render camera itself.
import * as THREE from 'three';
import { TransformControls } from 'three/addons/controls/TransformControls.js';

const app = window.app; // set by the page's main script
const $ = (id) => document.getElementById(id);
const canvas = $('preview');
const labelsEl = $('preview-labels');
// The preview's size in CSS px: the whole window on wide screens, the stage at the top on small
// ones (the page's stacked layout, the same media query as its CSS).
const viewW = () => canvas.clientWidth || innerWidth;
const viewH = () => canvas.clientHeight || innerHeight;
const stacked = matchMedia('(max-width: 900px), (max-height: 520px)');

const renderer = new THREE.WebGLRenderer({ canvas, antialias: true });
renderer.setPixelRatio(Math.min(devicePixelRatio, 2));
renderer.shadowMap.enabled = true;
const scene = new THREE.Scene();
// What the preview is seen through: its own view, or (looking through) the render camera.
const cam = new THREE.PerspectiveCamera(45, 16 / 9, 0.05, 500);
cam.up.set(0, 0, 1); // Z-up, as in the renderer
const LOOK = 'ray-tracer-look-through';
let lookThrough = false;
try { lookThrough = localStorage.getItem(LOOK) === '1'; } catch {}
/** The preview's own view: an orbit around `target`, like the render camera's (degrees). */
let view = null;

// Lighting roughly like the renderer's default: an overhead light and a soft blue-grey sky.
const hemi = new THREE.HemisphereLight(0xb8c8e0, 0x202024, 1.4);
hemi.position.set(0, 0, 1);
const sun = new THREE.DirectionalLight(0xffffff, 2.2);
sun.position.set(4, -5, 12);
sun.castShadow = true;
sun.shadow.mapSize.set(2048, 2048);
Object.assign(sun.shadow.camera, { left: -12, right: 12, top: 12, bottom: -12, near: 0.5, far: 40 });
sun.shadow.bias = -0.0005;
sun.shadow.normalBias = 0.03;
scene.add(hemi, sun, sun.target);

// The floor: the script's floor color, or (the default black void) a faint grid to place things on.
const floorMesh = new THREE.Mesh(new THREE.PlaneGeometry(40, 40), new THREE.MeshStandardMaterial({ roughness: 0.9 }));
floorMesh.receiveShadow = true;
const shadowCatcher = new THREE.Mesh(new THREE.PlaneGeometry(40, 40), new THREE.ShadowMaterial({ opacity: 0.35 }));
shadowCatcher.receiveShadow = true;
const grid = new THREE.GridHelper(40, 40, 0x3a3f4a, 0x1c1f26);
grid.rotation.x = Math.PI / 2;
scene.add(floorMesh, shadowCatcher, grid);

const world = new THREE.Group(); // the parts and volume boxes
scene.add(world);

// The render camera, as an object: a small body with a lens, and its frustum out to the target.
const renderCam = new THREE.PerspectiveCamera(40, 16 / 9, 0.2, 10);
renderCam.up.set(0, 0, 1);
const camBody = new THREE.Mesh(new THREE.BoxGeometry(0.34, 0.24, 0.3), new THREE.MeshStandardMaterial({ color: 0x8fb2f0, roughness: 0.5 }));
const lens = new THREE.Mesh(new THREE.CylinderGeometry(0.07, 0.1, 0.16, 20), new THREE.MeshStandardMaterial({ color: 0x1b2333, roughness: 0.3 }));
lens.rotation.x = Math.PI / 2; // along the view axis (-Z)
lens.position.z = -0.22;
camBody.add(lens);
renderCam.add(camBody);
const frustum = new THREE.CameraHelper(renderCam);
const accent = new THREE.Color(0x8fb2f0);
frustum.setColors(accent, accent, accent, accent, accent);
scene.add(renderCam, frustum);
const CAMERA = { label: 'camera', isCamera: true, labelEl: null }; // the camera, as a selection

const tc = new TransformControls(cam, canvas);
tc.setTranslationSnap(0.05);
tc.setRotationSnap(5 * Math.PI / 180);
tc.setSize(0.8);
scene.add(tc.getHelper());

let parts = [];   // { label, at, rotate, placed: { at, rotate, insert }, group, mesh, labelEl }
let handles = []; // { start, end, value }: where each `at` and `rotate` is written (UTF-16 offsets)
let selected = null; // a part, or CAMERA
let mode = 'translate'; // the gizmo: 'translate' (W) or 'rotate' (E)
let outline = null;
let synced = null;    // the script the meshes were built from (or that a drag wrote)
let requested = null; // the script last sent to the worker
let shown = false;

// --- The worker that runs and meshes scripts ------------------------------------------------
let worker, nextId = 0;
function startWorker() {
  worker = new Worker('preview-worker.js', { type: 'module' });
  worker.onmessage = (e) => onMeshed(e.data);
  worker.onerror = (e) => setNote(`Preview failed to load: ${e.message || 'see the console'}`, true);
}
startWorker();

function update() {
  const text = app.script();
  if (text === synced || text === requested) return;
  requested = text;
  if (!text.trim()) { clear(); setNote(''); synced = text; return; }
  setNote('Meshing…');
  worker.postMessage({ id: ++nextId, script: text });
}

function onMeshed(msg) {
  if (msg.crashed) {
    worker.terminate(); startWorker();
    setNote(`The preview crashed on this script (${msg.crashed})`, true);
    requested = null;
    return;
  }
  if (msg.id !== nextId) return; // a newer script is on its way
  if (msg.meta.error) { setNote(msg.meta.error, true); return; }
  build(msg.meta, msg.meshes);
  synced = requested;
  const count = parts.filter(movable).length;
  setNote(`${parts.length} part${parts.length === 1 ? '' : 's'} · ${count ? `${count} movable: click one, then move (W) or turn (E) it` : 'give parts a label and at: [x, y, z] to move them here'} · ${Math.round(msg.ms)} ms`);
}

// --- Building the scene ---------------------------------------------------------------------
const linear = (c) => new THREE.Color().setRGB(c[0], c[1], c[2], THREE.LinearSRGBColorSpace);

function clear() {
  select(null);
  world.traverse((o) => { o.geometry?.dispose(); o.material?.dispose?.(); });
  world.clear();
  labelsEl.replaceChildren();
  parts = [];
  handles = [];
  requestRender();
}

function build(meta, meshes) {
  const keep = selected && !selected.isCamera && selected.label;
  clear();
  handles = meta.handles;
  meta.parts.forEach((p, i) => {
    const m = meshes[i];
    const geometry = new THREE.BufferGeometry();
    geometry.setAttribute('position', new THREE.BufferAttribute(m.positions, 3));
    geometry.setAttribute('normal', new THREE.BufferAttribute(m.normals, 3));
    geometry.setIndex(new THREE.BufferAttribute(m.indices, 1));
    const shaded = m.colors.length > 0 && p.color && m.colors.some((v, j) => Math.abs(v - p.color[j % 3]) > 1e-3);
    if (shaded) geometry.setAttribute('color', new THREE.BufferAttribute(m.colors, 3));
    geometry.computeBoundingBox();
    const material = new THREE.MeshStandardMaterial({
      color: shaded ? 0xffffff : linear(p.color),
      vertexColors: shaded,
      roughness: p.roughness,
      metalness: p.metallic,
      transparent: p.glass,
      opacity: p.glass ? 0.35 : 1,
      side: THREE.DoubleSide,
    });
    if (p.emit > 0) { material.emissive = linear(p.color); material.emissiveIntensity = Math.min(p.emit / 3, 2); }
    const mesh = new THREE.Mesh(geometry, material);
    mesh.castShadow = !p.glass;
    mesh.receiveShadow = true;
    const group = new THREE.Group();
    group.position.fromArray(p.at);
    group.rotation.set(...p.rotate.map((d) => d * Math.PI / 180), 'ZYX');
    group.add(mesh);
    world.add(group);
    const part = { label: p.label, at: p.at, rotate: p.rotate, placed: p.placed, group, mesh, labelEl: null };
    mesh.userData.part = part;
    if (p.label) {
      const el = document.createElement('button');
      el.type = 'button';
      el.className = 'part-label' + (movable(part) ? '' : ' fixed');
      el.textContent = p.label;
      el.title = movable(part) ? 'Click to move (W) or turn (E)' : 'Its position is computed, so it can only be moved in the script';
      el.onclick = () => select(part);
      labelsEl.append(el);
      part.labelEl = el;
    }
    parts.push(part);
  });
  for (const v of meta.volumes) {
    const box = new THREE.Box3(new THREE.Vector3(...v.min), new THREE.Vector3(...v.max));
    world.add(new THREE.Box3Helper(box, 0xff8a3c));
  }
  const floor = meta.floor;
  floorMesh.visible = !!floor;
  if (floor) floorMesh.material.color = linear(floor);
  shadowCatcher.visible = grid.visible = !floor;
  scene.background = meta.sky ? linear(meta.sky) : null;
  // Keep the selection across re-meshes (e.g. after editing the script by hand).
  if (keep) select(parts.find((p) => p.label === keep && movable(p)) || null);
  else if (selected === CAMERA) select(CAMERA);
  requestRender();
}

// --- Selecting, moving and turning -----------------------------------------------------------
const movable = (p) => p.placed.at != null;
/** Whether a part's turn can be written: it has a `rotate` literal, or one can be inserted. */
const turnable = (p) => movable(p) && (p.placed.rotate != null || p.placed.insert != null);
/** The parts that move with `part`: those placed by the same `at` text. */
const sameGroup = (part) => parts.filter((p) => movable(p) && p.placed.at === part.placed.at);

function select(part) {
  if (part && !part.isCamera && !movable(part)) {
    setNote(`"${part.label ?? 'This part'}" is placed by a computed position, so move it in the script`, true);
    part = null;
  }
  selected = part;
  if (outline) { scene.remove(outline); outline.dispose(); outline = null; }
  const group = part && !part.isCamera ? sameGroup(part) : [];
  parts.forEach((p) => p.labelEl?.classList.toggle('selected', group.includes(p)));
  CAMERA.labelEl.classList.toggle('selected', part === CAMERA);
  if (part === CAMERA) {
    tc.attach(renderCam);
  } else if (part) {
    tc.attach(part.group);
    outline = new THREE.Box3Helper(new THREE.Box3(), 0xffd36b);
    scene.add(outline);
    fitOutline();
  } else {
    tc.detach();
  }
  setTool(mode);
}

/** The outline around everything that moves with the selection. */
function fitOutline() {
  if (!outline || !selected || selected.isCamera) return;
  outline.box.makeEmpty();
  for (const p of sameGroup(selected)) outline.box.union(new THREE.Box3().setFromObject(p.mesh));
}

let dragStart = null; // the selected part's turn when a drag began
tc.addEventListener('change', requestRender);
tc.addEventListener('mouseDown', () => {
  if (!selected || selected.isCamera) return;
  dragStart = selected.group.quaternion.clone();
  for (const p of sameGroup(selected)) p.turnStart = p.group.quaternion.clone();
});
tc.addEventListener('objectChange', () => {
  if (selected === CAMERA) return fromRig();
  if (!selected) return;
  // Parts placed by the same text move together; turning, they turn about that shared origin.
  const turn = dragStart && selected.group.quaternion.clone().multiply(dragStart.clone().invert());
  for (const p of sameGroup(selected)) {
    if (p === selected) continue;
    p.group.position.copy(selected.group.position);
    if (tc.mode === 'rotate' && turn) p.group.quaternion.copy(turn).multiply(p.turnStart);
  }
  fitOutline();
});
tc.addEventListener('dragging-changed', (e) => {
  if (e.value || selected === CAMERA) return;
  for (const p of parts) delete p.turnStart;
  dragStart = null;
  commit();
});

/**
 * The gizmo for the selection, `next`: 'translate' (arrows) or 'rotate' (rings). Parts turn about
 * the world axes, in all three; the camera turns about its own, without the view-axis ring, since
 * the renderer's camera can't roll.
 */
function setTool(next) {
  mode = next;
  const rotate = mode === 'rotate' && !!selected && (selected === CAMERA || turnable(selected));
  tc.setMode(rotate ? 'rotate' : 'translate');
  tc.setSpace(rotate && selected === CAMERA ? 'local' : 'world');
  tc.showZ = !(rotate && selected === CAMERA);
  document.querySelectorAll('[data-tool]').forEach((b) => {
    // The button names the view it switches to; looking through the camera, there's nothing
    // to move or turn (dragging orbits the camera itself), so those buttons go away.
    if (b.dataset.tool === 'look') {
      b.textContent = lookThrough ? 'World view' : 'Camera view';
      b.title = lookThrough ? 'Back to the scene, with the camera as an object you can move (C)' : 'See exactly what the render will frame (C)';
      return;
    }
    // Move and Turn appear only while something is selected (and Turn only if its turn can be
    // written into the script), rather than sitting there grayed out.
    const cantTurn = b.dataset.tool === 'rotate' && selected !== CAMERA && !!selected && !turnable(selected);
    b.hidden = lookThrough || !selected || cantTurn;
    b.disabled = false;
    b.setAttribute('aria-pressed', String(!!selected && b.dataset.tool === (rotate ? 'rotate' : 'translate')));
  });
  requestRender();
}

const round = (v, step) => Math.round(v / step) * step;
const fmt = (v) => (Number.isInteger(v) ? v.toFixed(1) : String(Math.round(v * 100) / 100));
const vec = (v) => `[${v.map(fmt).join(', ')}]`;
/** A part's turn in the script's terms: degrees about x, then y, then z (Euler order ZYX). */
const turnOf = (group) => {
  const e = new THREE.Euler().setFromQuaternion(group.quaternion, 'ZYX');
  return [e.x, e.y, e.z].map((r) => { const d = round(r / RAD, 0.1); return Object.is(d, -0) ? 0 : d; });
};

/** Writes where the selected part (and its group) now are, or how they're turned, into the script. */
function commit() {
  if (!selected) return;
  const script = app.script();
  if (synced !== script) {
    // Edited by hand since the last mesh: the offsets may be stale.
    setNote('The script changed; try again once it is re-meshed', true);
    return update();
  }
  const group = sameGroup(selected);
  const edits = [];
  if (tc.mode === 'translate') {
    const pos = selected.group.position.toArray().map((v) => round(v, 0.01));
    const h = handles[selected.placed.at];
    if (pos.some((v, i) => v !== h.value[i])) edits.push({ handle: selected.placed.at, text: vec(pos), value: pos });
    for (const p of group) p.at = pos;
  } else {
    const seen = new Set();
    for (const p of group) {
      const deg = turnOf(p.group);
      p.rotate = deg;
      if (p.placed.rotate != null) {
        if (seen.has(p.placed.rotate)) continue;
        seen.add(p.placed.rotate);
        const h = handles[p.placed.rotate];
        if (deg.some((v, i) => v !== h.value[i])) edits.push({ handle: p.placed.rotate, text: vec(deg), value: deg });
      } else if (p.placed.insert != null && deg.some((v) => v !== 0)) {
        edits.push({ insert: p, at: p.placed.insert, text: `, rotate: ${vec(deg)}`, value: deg });
      }
    }
  }
  if (!edits.length) return;
  synced = requested = applyEdits(script, edits);
  app.setScript(synced);
  fitOutline();
  setNote(tc.mode === 'translate'
    ? `Moved "${selected.label}" to ${edits[0].text}`
    : `Turned "${selected.label}" to ${vec(selected.rotate)} (degrees about x, then y, then z)`);
}

/**
 * Applies `edits` (each replaces a handle's text, or inserts `text` at `at`) to `script`, and
 * moves every other handle and insertion point to match. An insertion becomes the part's
 * `rotate` handle. Returns the new script.
 */
function applyEdits(script, edits) {
  const span = (e) => (e.handle != null ? [handles[e.handle].start, handles[e.handle].end] : [e.at, e.at]);
  const delta = (e) => { const [s, t] = span(e); return e.text.length - (t - s); };
  // How far text at `offset` moves: by every edit that ends at or before it (other than `self`).
  const shift = (offset, self) => edits.reduce((d, e) => d + (e !== self && span(e)[1] <= offset ? delta(e) : 0), 0);
  let next = script;
  for (const e of [...edits].sort((a, b) => span(b)[0] - span(a)[0])) {
    const [s, t] = span(e);
    next = next.slice(0, s) + e.text + next.slice(t);
  }
  const moved = handles.map((h, i) => {
    const e = edits.find((x) => x.handle === i);
    const start = h.start + shift(h.start, e);
    return { start, end: start + (e ? e.text.length : h.end - h.start), value: e ? e.value : h.value };
  });
  const inserted = new Map();
  for (const p of parts) {
    if (p.placed.insert == null) continue;
    const e = edits.find((x) => x.insert === p);
    const at = p.placed.insert + shift(p.placed.insert, e);
    if (!e) { p.placed.insert = at; continue; }
    const start = at + ', rotate: '.length;
    inserted.set(p, moved.push({ start, end: start + e.text.length - ', rotate: '.length, value: e.value }) - 1);
  }
  handles = moved;
  for (const [p, i] of inserted) { p.placed.rotate = i; p.placed.insert = null; }
  return next;
}

// --- The cameras ---------------------------------------------------------------------------
const RAD = Math.PI / 180;
/** How far the render camera can tilt (degrees above the target): the same whether it's turned as
 * an object or orbited while looking through it, within what the renderer accepts. */
const ELEVATION = [-80, 89];
/** Where an orbit camera sits: `distance` from `target`, at `azimuth` and `elevation` (degrees). */
const eyeOf = (c) => new THREE.Vector3(Math.cos(c.elevation * RAD) * Math.cos(c.azimuth * RAD),
  Math.cos(c.elevation * RAD) * Math.sin(c.azimuth * RAD), Math.sin(c.elevation * RAD)).multiplyScalar(c.distance).add(new THREE.Vector3(...c.target));

/** Sets `camera` up as the renderer will see: the render camera's position and aim, a field of
 * view across the longer side of the render. */
function asRenderCamera(camera) {
  const c = app.camera;
  camera.position.copy(eyeOf(c));
  camera.lookAt(new THREE.Vector3(...c.target));
  const { width: W, height: H } = app.resolution();
  const aspect = W / H;
  const half = (c.fov * RAD) / 2;
  camera.fov = (aspect >= 1 ? 2 * Math.atan(Math.tan(half) / aspect) : 2 * half) / RAD;
  camera.aspect = aspect;
}

/**
 * The part of the window the page's side panels leave uncovered, in px: the preview centers its
 * view there. On small screens the panels stack below the preview instead, so it's all of it.
 */
function openArea() {
  const area = { left: 0, right: viewW(), top: 0, bottom: viewH() };
  if (stacked.matches || document.body.classList.contains('hide-ui')) return area;
  const chat = document.getElementById('chat')?.getBoundingClientRect();
  const panel = document.getElementById('render-panel')?.getBoundingClientRect();
  if (chat?.width) area.left = chat.right;
  if (panel?.width) area.right = panel.left;
  if (area.right - area.left < 240) return { ...area, left: 0, right: viewW() };
  area.top = 130; // below the view switch and tools
  return area;
}

/** A view from the side and above that takes in both the render camera and what it looks at,
 * framed for the open part of the window. */
function defaultView() {
  const c = app.camera;
  const mid = eyeOf(c).add(new THREE.Vector3(...c.target)).multiplyScalar(0.5);
  const { left, right } = openArea();
  const narrow = Math.max(1, (0.9 * viewW()) / (right - left));
  return { target: mid.toArray(), azimuth: c.azimuth + 75, elevation: 32, distance: Math.min(90, (c.distance * 1.5 + 2.5) * narrow) };
}

function placeCamera() {
  if (lookThrough) {
    asRenderCamera(cam);
    // The render's resolution, cropped to cover the window as the page shows the render.
    const { width: W, height: H } = app.resolution();
    const [w, h] = [viewW(), viewH()];
    const s = Math.max(w / W, h / H);
    cam.setViewOffset(W * s, H * s, (W * s - w) / 2, (H * s - h) / 2, w, h);
  } else {
    view ??= defaultView();
    cam.position.copy(eyeOf(view));
    cam.lookAt(new THREE.Vector3(...view.target));
    cam.fov = 45;
    // Center the view in the open area: a wider frustum, cropped off-center.
    const { left, right } = openArea();
    const shift = (left + right) / 2 - viewW() / 2;
    const full = viewW() + 2 * Math.abs(shift);
    cam.aspect = full / viewH();
    cam.setViewOffset(full, viewH(), shift > 0 ? 0 : 2 * -shift, 0, viewW(), viewH());
  }
  cam.updateProjectionMatrix();
  // The camera object follows the render camera (unless it's being dragged: then it leads).
  if (!(tc.dragging && selected === CAMERA)) asRenderCamera(renderCam);
  renderCam.far = Math.max(0.3, app.camera.distance);
  renderCam.updateProjectionMatrix();
  renderCam.updateMatrixWorld();
  frustum.update();
  renderCam.visible = frustum.visible = !lookThrough;
}

/** The render camera, from the camera object as dragged: kept level (no roll), and aimed the same
 * distance ahead as before, where depth of field focuses. */
function fromRig() {
  const c = app.camera;
  const dir = renderCam.getWorldDirection(new THREE.Vector3());
  const elevation = Math.min(ELEVATION[1], Math.max(ELEVATION[0], Math.asin(Math.max(-1, Math.min(1, -dir.z))) / RAD));
  const azimuth = Math.atan2(-dir.y, -dir.x) / RAD;
  const back = eyeOf({ target: [0, 0, 0], azimuth, elevation, distance: c.distance });
  const target = renderCam.position.clone().sub(back).toArray().map((v) => Math.round(Math.max(-100, Math.min(100, v)) * 1000) / 1000);
  Object.assign(c, { target, azimuth: Math.round(azimuth * 100) / 100, elevation: Math.round(elevation * 100) / 100 });
  // Level it again right away, so the gizmo shows what the render will get.
  renderCam.lookAt(new THREE.Vector3(...target));
  app.cameraMoved(true);
  requestRender();
}

/** Orbits (dx, dy in pixels) and zooms the preview's view, or, looking through, the render camera. */
function orbit(dx, dy) {
  const c = lookThrough ? app.camera : (view ??= defaultView());
  c.azimuth = ((c.azimuth - dx * 0.4 + 540) % 360) - 180;
  const [lo, hi] = lookThrough ? ELEVATION : [-30, 89];
  c.elevation = Math.min(hi, Math.max(lo, c.elevation + dy * 0.3));
  if (lookThrough) app.cameraMoved(true);
  requestRender();
}
function zoom(factor) {
  const c = lookThrough ? app.camera : (view ??= defaultView());
  c.distance = Math.min(lookThrough ? 60 : 100, Math.max(1, c.distance * factor));
  if (lookThrough) app.cameraMoved(true);
  requestRender();
}
/** Moves the preview's view over the ground: `ahead` steps forward (+) or back, `side` right (+)
 * or left, each a tenth of the way to what it looks at (so steps shrink as you get closer). */
function walk(side, ahead) {
  view ??= defaultView();
  const az = view.azimuth * RAD;
  const forward = new THREE.Vector3(-Math.cos(az), -Math.sin(az), 0);
  const right = new THREE.Vector3(-Math.sin(az), Math.cos(az), 0);
  const step = Math.max(0.1, view.distance * 0.1);
  view.target = new THREE.Vector3(...view.target).addScaledVector(forward, ahead * step).addScaledVector(right, side * step).toArray();
  requestRender();
}

/** Slides the preview's view sideways and up/down (shift-drag). */
function pan(dx, dy) {
  if (lookThrough) return orbit(dx, dy);
  view ??= defaultView();
  const scale = (2 * view.distance * Math.tan((cam.fov * RAD) / 2)) / viewH();
  const right = new THREE.Vector3().setFromMatrixColumn(cam.matrixWorld, 0);
  const up = new THREE.Vector3().setFromMatrixColumn(cam.matrixWorld, 1);
  view.target = new THREE.Vector3(...view.target).addScaledVector(right, -dx * scale).addScaledVector(up, dy * scale).toArray();
  requestRender();
}

function setLookThrough(on) {
  if (tc.dragging || on === lookThrough) return;
  drag = null; // a background drag in progress belonged to the other view
  lookThrough = on;
  try { localStorage.setItem(LOOK, on ? '1' : '0'); } catch {}
  if (on && selected === CAMERA) select(null);
  // Looking through may have moved the camera anywhere: make sure it's in view again.
  if (!on) revealCamera = true;
  setTool(mode);
  requestRender();
}
let revealCamera = false;

/** Whether the render camera is in front of the preview's view, inside the open area. */
function cameraInView() {
  cam.updateMatrixWorld();
  const p = renderCam.position.clone().project(cam);
  const ahead = cam.getWorldDirection(new THREE.Vector3()).dot(renderCam.position.clone().sub(cam.position)) > 0;
  const [x, y] = [((p.x + 1) / 2) * viewW(), ((1 - p.y) / 2) * viewH()];
  const a = openArea();
  return ahead && x > a.left + 40 && x < a.right - 40 && y > a.top + 20 && y < a.bottom - 60;
}

let drag = null;
// Touch: one finger orbits, two pinch to zoom (and are never a click).
const touches = new Map();
let pinch = 0;
const spread = () => { const [a, b] = [...touches.values()]; return Math.hypot(a.x - b.x, a.y - b.y); };
canvas.addEventListener('pointerdown', (e) => {
  if (e.pointerType === 'touch') {
    touches.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (touches.size === 2) { pinch = spread(); drag = null; return; }
  }
  if (tc.axis) return; // on the gizmo: TransformControls drags the selection
  drag = { x: e.clientX, y: e.clientY, moved: false };
  canvas.setPointerCapture(e.pointerId);
});
const lift = (e) => {
  touches.delete(e.pointerId);
  if (touches.size < 2) pinch = 0;
};
canvas.addEventListener('pointercancel', (e) => { lift(e); drag = null; });
canvas.addEventListener('pointermove', (e) => {
  if (touches.has(e.pointerId)) touches.set(e.pointerId, { x: e.clientX, y: e.clientY });
  if (pinch && touches.size === 2) {
    const now = spread();
    if (now > 0) zoom(pinch / now);
    pinch = now;
    return;
  }
  if (!drag || tc.dragging) return;
  const [dx, dy] = [e.clientX - drag.x, e.clientY - drag.y];
  if (!drag.moved && Math.hypot(dx, dy) < 4) return;
  drag.moved = true;
  drag.x = e.clientX; drag.y = e.clientY;
  (e.shiftKey ? pan : orbit)(dx, dy);
});
canvas.addEventListener('pointerup', (e) => {
  lift(e);
  const click = drag && !drag.moved;
  drag = null;
  if (!click || tc.dragging) return;
  const r = canvas.getBoundingClientRect();
  const ndc = new THREE.Vector2(((e.clientX - r.left) / viewW()) * 2 - 1, -((e.clientY - r.top) / viewH()) * 2 + 1);
  const ray = new THREE.Raycaster();
  ray.setFromCamera(ndc, cam);
  const targets = parts.map((p) => p.mesh);
  if (!lookThrough) targets.push(camBody, lens);
  const hit = ray.intersectObjects(targets, false)[0];
  select(!hit ? null : hit.object === camBody || hit.object === lens ? CAMERA : hit.object.userData.part);
});
canvas.addEventListener('wheel', (e) => {
  e.preventDefault();
  zoom(Math.exp(e.deltaY * 0.001));
}, { passive: false });
addEventListener('keydown', (e) => {
  if (!shown || e.target.matches('input, select, textarea') || e.metaKey || e.ctrlKey || e.altKey) return;
  const key = e.key.toLowerCase();
  if (key === 'escape') select(null);
  else if (key === 'c') setLookThrough(!lookThrough);
  else if (key === 'w' || key === 'e') setTool(key === 'w' ? 'translate' : 'rotate');
  else if (ARROWS[e.key]) {
    e.preventDefault();
    // Arrows move the preview's own view; like Blender, that leaves the render camera's view.
    if (lookThrough) setLookThrough(false);
    const [side, ahead] = ARROWS[e.key];
    if (e.shiftKey) orbit(-side * 25, -ahead * 25); // turn and tilt
    else walk(side, ahead);
  }
});
const ARROWS = { ArrowUp: [0, 1], ArrowDown: [0, -1], ArrowLeft: [-1, 0], ArrowRight: [1, 0] };
document.querySelectorAll('[data-tool]').forEach((b) => (b.onclick = () => {
  if (b.dataset.tool === 'look') setLookThrough(!lookThrough);
  else setTool(b.dataset.tool);
}));

// The camera's own label: click it to select the camera.
CAMERA.labelEl = Object.assign(document.createElement('button'), {
  type: 'button', className: 'part-label camera', textContent: 'camera', title: 'Click to move (W) or turn (E) the render camera',
  onclick: () => select(CAMERA),
});

// --- Drawing --------------------------------------------------------------------------------
let frame = 0;
function requestRender() {
  if (shown && !frame) frame = requestAnimationFrame(draw);
}
function draw() {
  frame = 0;
  renderer.setSize(viewW(), viewH(), false);
  placeCamera();
  if (revealCamera && !lookThrough) {
    revealCamera = false;
    if (!cameraInView()) { view = defaultView(); placeCamera(); }
  }
  // The camera object keeps a readable size on screen (about 30 px tall), however far away it is.
  const size = (cam.position.distanceTo(renderCam.position) * 2 * Math.tan((cam.fov * RAD) / 2) * 30) / viewH() / 0.24;
  camBody.scale.setScalar(Math.max(0.2, size));
  renderer.render(scene, cam);
  // Labels sit above each part's box, and above the camera.
  const box = new THREE.Box3(), v = new THREE.Vector3();
  const place = (el, point, show = true) => {
    point.project(cam);
    const visible = show && point.z < 1 && Math.abs(point.x) < 1.2 && Math.abs(point.y) < 1.2;
    el.hidden = !visible;
    if (visible) el.style.transform = `translate(${(point.x + 1) / 2 * viewW()}px, ${(1 - point.y) / 2 * viewH()}px) translate(-50%, -130%)`;
  };
  for (const p of parts) {
    if (!p.labelEl) continue;
    box.setFromObject(p.mesh);
    place(p.labelEl, v.set((box.min.x + box.max.x) / 2, (box.min.y + box.max.y) / 2, box.max.z), !box.isEmpty());
  }
  if (!CAMERA.labelEl.isConnected) labelsEl.append(CAMERA.labelEl);
  place(CAMERA.labelEl, renderCam.position.clone().add(new THREE.Vector3(0, 0, 0.2 * camBody.scale.x)), !lookThrough);
}
addEventListener('resize', requestRender);
// Hiding or showing the page's panels changes the open area the view is centered in.
new MutationObserver(requestRender).observe(document.body, { attributes: true, attributeFilter: ['class'] });

function setNote(text, error = false) {
  $('preview-note').textContent = text;
  $('preview-note').classList.toggle('err', error);
}

// --- Wiring to the page ---------------------------------------------------------------------
document.addEventListener('script-changed', update);
document.addEventListener('camera-changed', (e) => {
  if (e.detail === 'framed') view = null; // a new shape: look at it afresh
  requestRender();
});
document.addEventListener('view-changed', (e) => {
  shown = e.detail === 'preview';
  if (shown) { update(); requestRender(); } else select(null);
});
shown = document.body.classList.contains('previewing');
setTool(mode);
update();
requestRender();
