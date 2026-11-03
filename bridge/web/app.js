// The bridge's web UI. Plain ES module, no build step, like the frame's page.

// The frame portal's own converter, served by the bridge from tools/el6.mjs.
import { WIDTH, HEIGHT, FRAME_BYTES, packImage, drawFrame } from "./el6.mjs";

const $ = (id) => document.getElementById(id);

// Every POST carries this header; the bridge refuses a POST without it, which is
// what keeps another site's page from changing settings through your browser.
function post(url, body) {
  return fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json", "X-Easellet": "1" },
    body: JSON.stringify(body),
  });
}

// A raw picture (or an empty body) goes through the same header.
function postBytes(url, bytes) {
  return fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/octet-stream", "X-Easellet": "1" },
    body: bytes ?? new Uint8Array(0),
  });
}

function say(id, text, kind) {
  const element = $(id);
  element.textContent = text;
  element.className = `message ${kind ?? ""}`;
}

// ---- tabs ------------------------------------------------------------------

const TABS = ["frames", "plugins", "assistant", "settings"];

function showTab() {
  const wanted = location.hash.slice(1);
  const current = TABS.includes(wanted) ? wanted : "frames";
  for (const name of TABS) {
    $(`tab-${name}`).hidden = name !== current;
    $(`tab-link-${name}`).setAttribute("aria-selected", String(name === current));
  }
  if (current === "frames") loadFrames();
  if (current === "plugins") {
    // The job list names a plugin by the title its manifest carries, so it waits
    // for the plugin list instead of racing it to the render.
    loadPlugins().then(loadJobs);
  }
  if (current === "assistant") loadHistory();
  if (current === "settings") loadSettings();
}
addEventListener("hashchange", showTab);

// ---- status ----------------------------------------------------------------

async function loadStatus() {
  try {
    const answer = await fetch("/api/status");
    if (!answer.ok) throw new Error(answer.status);
    const status = await answer.json();
    $("link-dot").className = "dot on";
    $("link-text").textContent = t("link.online", { uptime: uptime(status.uptime_s) });
    $("backends-chip").textContent =
      `${status.backends.stt} · ${short(status.backends.llm)} · ${short(status.backends.tts)}`;
    $("version").textContent = status.version;
  } catch {
    $("link-dot").className = "dot off";
    $("link-text").textContent = t("link.down");
  }
}

function short(spec) {
  // "cmd:bridge/scripts/llm-local.py" -> "llm-local.py"
  return spec.startsWith("cmd:") ? spec.slice(4).split(/\s/)[0].split("/").pop() : spec;
}

function uptime(seconds) {
  if (seconds < 120) return t("uptime.s", { n: seconds });
  if (seconds < 7200) return t("uptime.min", { n: Math.round(seconds / 60) });
  return t("uptime.h", { n: Math.round(seconds / 3600) });
}

// ---- frames ----------------------------------------------------------------

async function loadFrames(scan = false) {
  $("frames-note").textContent = scan ? t("frames.searching") : "";
  try {
    const answer = scan ? await postBytes("/api/frames/scan") : await fetch("/api/frames");
    if (!answer.ok) throw new Error((await answer.text()).trim());
    const { frames } = await answer.json();
    renderFrames(frames);
    if (!exposureDirty) loadExposure(frames);
    $("frames-note").textContent = tCount("frames.count", frames.length);
  } catch (error) {
    say("frames-message", t("frames.failed", { error: error.message }), "bad");
    $("frames-note").textContent = "";
  }
}

function ago(seconds) {
  if (seconds == null) return t("frames.ago.never");
  if (seconds < 90) return t("frames.ago.online");
  if (seconds < 5400) return t("frames.ago.min", { n: Math.round(seconds / 60) });
  return t("frames.ago.hour", { n: Math.round(seconds / 3600) });
}

// Frame cards are rebuilt on every scan, but an open gallery or a prepared
// picture must survive that; so cards are kept by id and only their header redrawn.
const cards = new Map();

function renderFrames(frames) {
  const list = $("frame-list");
  if (!frames.length) {
    list.replaceChildren(text("p", t("frames.empty"), "empty"));
    cards.clear();
    return;
  }
  const seen = new Set();
  for (const frame of frames) {
    seen.add(frame.id);
    let card = cards.get(frame.id);
    if (!card) {
      card = buildCard(frame);
      cards.set(frame.id, card);
    }
    card.update(frame);
  }
  for (const [id, card] of cards) {
    if (!seen.has(id)) {
      card.element.remove();
      cards.delete(id);
    }
  }
  list.replaceChildren(...frames.map((frame) => cards.get(frame.id).element));
}

function chip(content, className) {
  return text("span", content, `chip ${className ?? ""}`);
}

// ---- the bridge as an OpenDisplay device -----------------------------------

// `lastFrames` lets the exposure controls redraw after a save without another
// network scan. `exposureDirty` stops the 30 s refresh from replacing a choice
// the user is still editing.
let lastFrames = [];
let exposureDirty = false;

async function loadExposure(frames = lastFrames) {
  lastFrames = frames;
  try {
    const answer = await fetch("/api/opendisplay");
    if (!answer.ok) throw new Error((await answer.text()).trim());
    renderExposure(await answer.json());
  } catch (error) {
    $("expose-status").textContent = t("expose.failed", { error: error.message });
    $("expose-status").className = "note bad";
  }
}

function renderExposure(data) {
  $("expose-enabled").checked = Boolean(data.enabled);
  const select = $("expose-frame");
  const chosen = data.frame || "";
  select.replaceChildren(new Option(t("expose.choose"), ""));
  for (const frame of lastFrames) {
    select.append(new Option(`${frame.name} (${frame.host})`, frame.id));
  }
  if (chosen && !lastFrames.some((frame) => frame.id === chosen)) {
    select.append(new Option(`${chosen} (${t("expose.missing")})`, chosen));
  }
  select.value = chosen;
  $("expose-name").value = data.name || "";
  const status = $("expose-status");
  status.className = data.error ? "note bad" : "note";
  status.textContent = data.running
    ? t("expose.running", { instance: data.instance, port: data.port })
    : t("expose.stopped");
  if (data.error) status.textContent += ` ${data.error}`;
}

async function saveExposure() {
  const request = {
    enabled: $("expose-enabled").checked,
    frame: $("expose-frame").value,
    name: $("expose-name").value.trim(),
  };
  if (request.enabled && !request.frame) {
    say("frames-message", t("expose.chooseError"), "bad");
    return;
  }
  const answer = await post("/api/opendisplay", request);
  if (!answer.ok) {
    say("frames-message", (await answer.text()).trim(), "bad");
    return;
  }
  exposureDirty = false;
  renderExposure(await answer.json());
  say("frames-message", t("expose.saved"), "ok");
  setTimeout(() => loadExposure(), 1200);
}

for (const id of ["expose-enabled", "expose-frame", "expose-name"]) {
  $(id).addEventListener("input", () => {
    exposureDirty = true;
  });
  $(id).addEventListener("change", () => {
    exposureDirty = true;
  });
}
$("expose-save").addEventListener("click", saveExposure);

function buildCard(initial) {
  let frame = initial;
  const element = document.createElement("article");
  element.className = "frame-card";
  const header = document.createElement("header");
  const chips = document.createElement("div");
  chips.className = "chips";

  // Send a picture: the browser converts, exactly as on the frame's own page.
  const send = document.createElement("div");
  send.className = "send";
  const controls = document.createElement("div");
  controls.className = "field";
  const file = document.createElement("input");
  file.type = "file";
  file.accept = "image/*";
  const mode = document.createElement("select");
  mode.append(new Option(t("card.modeCover"), "cover"), new Option(t("card.modeContain"), "contain"));
  const dither = document.createElement("label");
  dither.className = "option";
  const ditherBox = document.createElement("input");
  ditherBox.type = "checkbox";
  ditherBox.checked = true;
  dither.append(ditherBox, ` ${t("card.dither")}`);
  const show = text("button", t("card.showNow"), "primary");
  const store = text("button", t("card.saveGallery"));
  show.disabled = store.disabled = true;
  const buttons = document.createElement("div");
  buttons.className = "row";
  buttons.append(show, store);
  const message = text("p", "", "message");
  controls.append(file, mode, dither, buttons, message);
  const canvas = document.createElement("canvas");
  canvas.width = WIDTH;
  canvas.height = HEIGHT;
  canvas.hidden = true;
  const context = canvas.getContext("2d", { willReadFrequently: true });
  send.append(controls, canvas);

  let picture = null;
  let packed = null;
  async function prepare() {
    if (!picture) return;
    packed = packImage(picture, context, { mode: mode.value, dither: ditherBox.checked });
    drawFrame(context, packed);
    canvas.hidden = false;
    show.disabled = !frame.panel;
    store.disabled = !frame.portal;
  }
  file.addEventListener("change", async () => {
    const chosen = file.files?.[0];
    if (!chosen) return;
    try {
      picture = await createImageBitmap(chosen);
      await prepare();
      message.textContent = "";
    } catch (error) {
      message.textContent = t("card.notPicture", { error: error.message });
      message.className = "message bad";
    }
  });
  mode.addEventListener("change", prepare);
  ditherBox.addEventListener("change", prepare);

  async function deliver(action, button) {
    if (!packed) return;
    button.disabled = true;
    message.textContent = action === "display" ? t("card.sending") : t("card.saving");
    message.className = "message";
    try {
      const answer = await postBytes(`/api/frames/${frame.id}/${action}`, packed);
      const reply = (await answer.text()).trim();
      message.textContent = reply;
      message.className = answer.ok ? "message ok" : "message bad";
      if (answer.ok && action === "upload" && !gallery.hidden) loadGallery();
    } catch (error) {
      message.textContent = error.message;
      message.className = "message bad";
    } finally {
      button.disabled = false;
    }
  }
  show.addEventListener("click", () => deliver("display", show));
  store.addEventListener("click", () => deliver("upload", store));

  // The frame's gallery, through the bridge.
  const galleryToggle = text("button", t("card.gallery"));
  const remove = text("button", t("card.removeFrame"), "danger");
  const gallery = document.createElement("div");
  gallery.className = "thumbs";
  gallery.hidden = true;
  galleryToggle.addEventListener("click", () => {
    gallery.hidden = !gallery.hidden;
    if (!gallery.hidden) loadGallery();
  });
  remove.addEventListener("click", async () => {
    const answer = await post("/api/frames/remove", { host: frame.host });
    if (answer.ok) renderFrames((await answer.json()).frames);
    else say("frames-message", (await answer.text()).trim(), "bad");
  });

  async function loadGallery() {
    gallery.replaceChildren(text("p", t("card.loading"), "empty"));
    try {
      const answer = await fetch(`/api/frames/${frame.id}/gallery`);
      if (!answer.ok) throw new Error((await answer.text()).trim());
      const { images } = await answer.json();
      if (!images.length) {
        gallery.replaceChildren(text("p", t("card.galleryEmpty"), "empty"));
        return;
      }
      gallery.replaceChildren(...images.map(thumbnail));
    } catch (error) {
      gallery.replaceChildren(text("p", t("card.galleryDown", { error: error.message }), "empty"));
    }
  }

  function thumbnail(image) {
    const figure = document.createElement("figure");
    const small = document.createElement("canvas");
    small.width = WIDTH;
    small.height = HEIGHT;
    const name = image.name.split("/").pop();
    const line = document.createElement("div");
    line.className = "line";
    const showIt = text("button", t("card.showOne"));
    const deleteIt = text("button", t("card.delete"), "danger");
    // The frame refuses deletion outside its writable directory anyway; the
    // button is only offered where it would succeed.
    deleteIt.hidden = !image.name.toUpperCase().startsWith("EASELLET/");
    line.append(text("span", name), showIt, deleteIt);
    figure.append(small, line);
    const query = `path=${encodeURIComponent(image.name)}`;
    fetch(`/api/frames/${frame.id}/image?${query}`)
      .then((answer) => (answer.ok ? answer.arrayBuffer() : Promise.reject()))
      .then((bytes) => {
        const raw = new Uint8Array(bytes);
        if (raw.length === FRAME_BYTES) drawFrame(small.getContext("2d"), raw);
      })
      .catch(() => {});
    showIt.addEventListener("click", async () => {
      const answer = await postBytes(`/api/frames/${frame.id}/show?${query}`);
      message.textContent = (await answer.text()).trim();
      message.className = answer.ok ? "message ok" : "message bad";
    });
    deleteIt.addEventListener("click", async () => {
      // Two clicks, in place, as on the frame's page.
      if (!deleteIt.classList.contains("armed")) {
        deleteIt.classList.add("armed");
        deleteIt.textContent = t("card.confirm");
        setTimeout(() => {
          deleteIt.classList.remove("armed");
          deleteIt.textContent = t("card.delete");
        }, 4000);
        return;
      }
      const answer = await postBytes(`/api/frames/${frame.id}/delete?${query}`);
      if (answer.ok) figure.remove();
      message.textContent = (await answer.text()).trim();
      message.className = answer.ok ? "message ok" : "message bad";
    });
    return figure;
  }

  // A message card, drawn by the bridge in the panel's own colours.
  const note = document.createElement("details");
  note.className = "note-card";
  const noteSummary = text("summary", t("note.summary"));
  const noteTitle = document.createElement("input");
  noteTitle.type = "text";
  noteTitle.placeholder = t("note.title");
  const noteBody = document.createElement("textarea");
  noteBody.placeholder = t("note.body");
  const noteAccent = document.createElement("select");
  for (const value of ["blue", "red", "green", "yellow", "black"]) {
    noteAccent.append(new Option(t(`color.${value}`), value));
  }
  const notePreview = text("button", t("note.preview"));
  const noteShow = text("button", t("note.show"), "primary");
  const noteImage = document.createElement("img");
  noteImage.className = "preview";
  noteImage.alt = t("note.alt");
  noteImage.hidden = true;
  const noteMessage = text("p", "", "message");
  const noteButtons = document.createElement("div");
  noteButtons.className = "row";
  noteButtons.append(noteAccent, notePreview, noteShow);
  const noteFields = document.createElement("div");
  noteFields.className = "field";
  noteFields.append(noteTitle, noteBody, noteButtons, noteMessage);
  note.append(noteSummary, noteFields, noteImage);
  const card = () => ({
    layout: "notice",
    title: noteTitle.value.trim() || t("note.defaultTitle"),
    body: noteBody.value,
    accent: noteAccent.value,
  });
  notePreview.addEventListener("click", async () => {
    const answer = await post("/api/render/preview", card());
    if (!answer.ok) {
      noteMessage.textContent = (await answer.text()).trim();
      noteMessage.className = "message bad";
      return;
    }
    if (noteImage.src) URL.revokeObjectURL(noteImage.src);
    noteImage.src = URL.createObjectURL(await answer.blob());
    noteImage.hidden = false;
    noteMessage.textContent = "";
  });
  noteShow.addEventListener("click", async () => {
    noteShow.disabled = true;
    noteMessage.textContent = t("card.sending");
    noteMessage.className = "message";
    try {
      const answer = await post(`/api/frames/${frame.id}/card`, card());
      noteMessage.textContent = (await answer.text()).trim();
      noteMessage.className = answer.ok ? "message ok" : "message bad";
    } finally {
      noteShow.disabled = !frame.panel;
    }
  });

  // The screen: which job fills which rectangle of this frame.
  const screenBox = document.createElement("details");
  screenBox.className = "note-card screen-card";
  const screenSummary = text("summary", t("screen.summary"));
  const screenBody = document.createElement("div");
  screenBody.className = "screen-body";
  const screenNote = text("p", t("screen.note"), "note");
  const presetField = document.createElement("div");
  presetField.className = "field";
  const presetLabel = text("label", t("screen.preset"));
  presetLabel.htmlFor = `screen-preset-${initial.id}`;
  const preset = document.createElement("select");
  preset.id = `screen-preset-${initial.id}`;
  const PRESETS = {
    single: ["a"],
    stack: ["a a", "b b"],
    columns: ["a b"],
    hero: ["a a", "b c"],
    grid: ["a b", "c d"],
  };
  for (const name of Object.keys(PRESETS)) {
    preset.append(new Option(t(`screen.presets.${name}`), name));
  }
  presetField.append(presetLabel, preset);
  const map = document.createElement("div");
  map.className = "preset-map";
  map.setAttribute("aria-hidden", "true");
  const slots = document.createElement("div");
  slots.className = "screen-slots";
  const screenSave = text("button", t("screen.save"), "primary");
  const screenPreview = text("button", t("screen.preview"));
  const screenShow = text("button", t("screen.show"));
  const screenButtons = document.createElement("div");
  screenButtons.className = "row";
  screenButtons.append(screenSave, screenPreview, screenShow);
  const screenMessage = text("p", "", "message");
  const screenImage = document.createElement("img");
  screenImage.className = "preview";
  screenImage.alt = t("screen.previewAlt");
  screenImage.hidden = true;
  screenBody.append(
    screenNote,
    presetField,
    map,
    slots,
    screenButtons,
    screenMessage,
    screenImage,
  );
  screenBox.append(screenSummary, screenBody);

  let jobsForFrame = [];
  const areas = () => PRESETS[preset.value] ?? PRESETS.single;
  const slotNames = () => [...new Set(areas().join(" ").split(" "))];

  function drawMap() {
    const rows = areas();
    const columns = rows[0].split(" ").length;
    map.style.gridTemplateAreas = rows.map((row) => `"${row}"`).join(" ");
    map.style.gridTemplateRows = `repeat(${rows.length}, 1fr)`;
    map.style.gridTemplateColumns = `repeat(${columns}, 1fr)`;
    map.replaceChildren(
      ...slotNames().map((area) => {
        const cell = document.createElement("span");
        cell.style.gridArea = area;
        return cell;
      }),
    );
  }

  function jobName(job) {
    return `${job.title || pluginTitles.get(job.plugin) || job.plugin} · ${job.id}`;
  }

  function buildSlots(values = []) {
    slots.replaceChildren(
      ...slotNames().map((_, index) => {
        const field = document.createElement("div");
        field.className = "field";
        const label = text("label", t("screen.slot", { n: index + 1 }));
        const select = document.createElement("select");
        select.append(new Option(t("screen.empty"), ""));
        for (const job of jobsForFrame) select.append(new Option(jobName(job), job.id));
        const chosen = values[index] ?? "";
        if (chosen && !jobsForFrame.some((job) => job.id === chosen)) {
          select.append(new Option(`${chosen} (${t("screen.missing")})`, chosen));
        }
        select.value = chosen;
        field.append(label, select);
        return field;
      }),
    );
  }

  async function loadScreen() {
    screenMessage.textContent = t("screen.loading");
    screenMessage.className = "message";
    try {
      const [jobAnswer, screenAnswer] = await Promise.all([
        fetch("/api/jobs"),
        fetch("/api/screens"),
      ]);
      if (!jobAnswer.ok || !screenAnswer.ok) throw new Error(t("load.failedShort"));
      const { jobs } = await jobAnswer.json();
      const { screens } = await screenAnswer.json();
      jobsForFrame = jobs.filter(
        (job) => job.mode !== "gallery" && job.frames.includes(frame.id),
      );
      const saved = screens.find((screen) => screen.frame === frame.id);
      preset.value = saved?.preset ?? "single";
      drawMap();
      buildSlots(saved?.slots ?? []);
      screenMessage.className = "message";
      screenMessage.textContent = jobsForFrame.length ? "" : t("screen.noJobs");
    } catch (error) {
      screenMessage.textContent = error.message;
      screenMessage.className = "message bad";
    }
  }

  preset.addEventListener("change", () => {
    const values = [...slots.querySelectorAll("select")].map((select) => select.value);
    drawMap();
    buildSlots(values);
  });

  screenSave.addEventListener("click", async () => {
    screenSave.disabled = true;
    const values = [...slots.querySelectorAll("select")].map((select) => select.value);
    try {
      const answer = await post("/api/screens/save", {
        frame: frame.id,
        preset: preset.value,
        slots: values,
      });
      const reply = (await answer.text()).trim();
      screenMessage.textContent = answer.ok ? t("screen.saved") : reply;
      screenMessage.className = answer.ok ? "message ok" : "message bad";
    } catch (error) {
      screenMessage.textContent = error.message;
      screenMessage.className = "message bad";
    } finally {
      screenSave.disabled = false;
    }
  });

  screenPreview.addEventListener("click", async () => {
    screenMessage.textContent = t("screen.loading");
    screenMessage.className = "message";
    try {
      const answer = await fetch(`/api/frames/${frame.id}/screen.png`, { cache: "no-store" });
      if (answer.status === 204) {
        screenMessage.textContent = t("screen.nothing");
        return;
      }
      if (!answer.ok) throw new Error((await answer.text()).trim());
      if (screenImage.src) URL.revokeObjectURL(screenImage.src);
      screenImage.src = URL.createObjectURL(await answer.blob());
      screenImage.hidden = false;
      screenMessage.textContent = "";
    } catch (error) {
      screenMessage.textContent = error.message;
      screenMessage.className = "message bad";
    }
  });

  screenShow.addEventListener("click", async () => {
    screenShow.disabled = true;
    screenMessage.textContent = t("card.sending");
    try {
      const answer = await postBytes(`/api/frames/${frame.id}/screen`);
      screenMessage.textContent = (await answer.text()).trim();
      screenMessage.className = answer.ok ? "message ok" : "message bad";
    } finally {
      screenShow.disabled = false;
    }
  });

  screenBox.addEventListener("toggle", () => {
    if (screenBox.open) loadScreen();
  });

  // The firmware: what the frame runs, what the release list offers, and the one
  // button in this page that writes flash. It asks twice, in place, like the
  // other destructive actions do.
  const firmwareBox = document.createElement("details");
  firmwareBox.className = "note-card firmware-card";
  const firmwareSummary = text("summary", t("firmware.summary"));
  const firmwareBody = document.createElement("div");
  firmwareBody.className = "screen-body";
  const firmwareLine = text("p", "", "note");
  const firmwareNotes = text("button", t("firmware.notes"));
  const firmwareInstall = text("button", t("firmware.install", { version: "…" }), "primary");
  const firmwareButtons = document.createElement("div");
  firmwareButtons.className = "row";
  firmwareButtons.append(firmwareNotes, firmwareInstall);
  const firmwareMessage = text("p", "", "message");
  firmwareBody.append(firmwareLine, firmwareButtons, firmwareMessage);
  firmwareBox.append(firmwareSummary, firmwareBody);

  let offered = null;
  let armed = null;
  let poll = null;

  function phaseText(state) {
    const label = t(`firmware.phase.${state.phase}`);
    const text = label === `firmware.phase.${state.phase}` ? "" : label;
    return [text, state.message].filter(Boolean).join(" · ");
  }

  function drawFirmware(ota, list, state) {
    const running = ota?.version ?? null;
    const release = list?.store?.index?.releases?.[0] ?? null;
    offered = release && release.version !== running ? release : null;
    const enabled = list?.enabled === true;

    if (!enabled) {
      firmwareLine.textContent = t("firmware.off");
    } else {
      const parts = [];
      parts.push(
        running
          ? t("firmware.running", { version: running, slot: ota.running ?? "" })
          : t("firmware.noFrame"),
      );
      if (!release) parts.push(t("firmware.noList"));
      else if (!offered) parts.push(t("firmware.uptodate"));
      else parts.push(t("firmware.available", { version: release.version }));
      firmwareLine.textContent = parts.join(" · ");
    }
    firmwareNotes.hidden = !(offered?.notes || release?.notes);
    firmwareInstall.hidden = !offered;
    if (offered) firmwareInstall.textContent = t("firmware.install", { version: offered.version });
    firmwareInstall.disabled = Boolean(state && state.phase !== "idle" && state.phase !== "done" && state.phase !== "failed");

    const busy = state && ["downloading", "writing", "restarting"].includes(state.phase);
    if (busy) {
      firmwareMessage.textContent = phaseText(state);
      firmwareMessage.className = "message";
    } else if (state?.phase === "done") {
      firmwareMessage.textContent = phaseText(state);
      firmwareMessage.className = "message ok";
    } else if (state?.phase === "failed") {
      firmwareMessage.textContent = phaseText(state);
      firmwareMessage.className = "message bad";
    } else {
      firmwareMessage.textContent = "";
      firmwareMessage.className = "message";
    }
    return busy;
  }

  async function readFirmware() {
    const [otaAnswer, listAnswer, stateAnswer] = await Promise.all([
      fetch(`/api/frames/${frame.id}/ota`),
      fetch("/api/firmware"),
      fetch(`/api/frames/${frame.id}/firmware`),
    ]);
    const ota = otaAnswer.ok ? await otaAnswer.json() : null;
    const list = listAnswer.ok ? await listAnswer.json() : null;
    const state = stateAnswer.ok ? await stateAnswer.json() : null;
    return { ota, list, state };
  }

  async function loadFirmware() {
    firmwareMessage.textContent = "";
    firmwareMessage.className = "message";
    try {
      const { ota, list, state } = await readFirmware();
      if (drawFirmware(ota, list, state)) pollState();
    } catch (error) {
      firmwareLine.textContent = t("firmware.noFrame");
      firmwareMessage.textContent = error.message;
      firmwareMessage.className = "message bad";
    }
  }

  // Writing flash and restarting take minutes; the page asks the bridge, not the
  // frame, and stops as soon as the answer is final.
  function pollState() {
    if (poll) clearInterval(poll);
    poll = setInterval(async () => {
      if (!firmwareBox.isConnected) {
        clearInterval(poll);
        poll = null;
        return;
      }
      try {
        const { ota, list, state } = await readFirmware();
        if (!drawFirmware(ota, list, state)) {
          clearInterval(poll);
          poll = null;
          loadFrames();
          loadFirmware();
        }
      } catch {
        /* the bridge is busy or restarting; ask again next tick */
      }
    }, 1500);
  }

  firmwareInstall.addEventListener("click", async () => {
    if (!offered) return;
    if (armed !== offered.version) {
      // First press: ask again, in place, for five seconds.
      armed = offered.version;
      firmwareInstall.textContent = t("firmware.pressAgain");
      setTimeout(() => {
        if (armed === offered?.version && offered) {
          armed = null;
          firmwareInstall.textContent = t("firmware.install", { version: offered.version });
        }
      }, 5000);
      return;
    }
    armed = null;
    firmwareInstall.disabled = true;
    firmwareMessage.textContent = t("firmware.starting");
    firmwareMessage.className = "message";
    const answer = await post(`/api/frames/${frame.id}/firmware`, {
      version: offered.version,
      confirm: true,
    });
    if (!answer.ok) {
      firmwareMessage.textContent = (await answer.text()).trim();
      firmwareMessage.className = "message bad";
      firmwareInstall.disabled = false;
      firmwareInstall.textContent = t("firmware.install", { version: offered.version });
      return;
    }
    pollState();
  });

  firmwareNotes.addEventListener("click", () => {
    const url = offered?.notes || "";
    if (url) window.open(url, "_blank", "noopener");
  });

  firmwareBox.addEventListener("toggle", () => {
    if (firmwareBox.open) loadFirmware();
  });

  const actions = document.createElement("div");
  actions.className = "row";
  element.append(header, chips, actions, send, note, screenBox, firmwareBox, gallery);

  function update(next) {
    frame = next;
    const title = document.createElement("div");
    title.append(text("h3", frame.name), text("div", `${frame.host} · ${frame.mac ?? t("frames.noMac")}${frame.firmware ? ` · fw ${frame.firmware}` : ""}`, "where"));
    const state = document.createElement("div");
    state.className = "chips";
    const dot = document.createElement("span");
    dot.className = frame.online ? "dot on" : "dot off";
    const link = chip("", "");
    link.append(dot, ` ${frame.online ? t("frames.ago.online") : ago(frame.seen_s)}`);
    state.append(link);
    header.replaceChildren(title, state);

    const parts = [];
    const sensors = frame.sensors;
    if (sensors?.battery?.present) {
      const battery = sensors.battery;
      const extra = battery.charging ? t("sensors.charging") : battery.usb ? t("sensors.usb") : "";
      parts.push(
        chip(
          t("sensors.battery", { percent: battery.percent, volts: (battery.mv / 1000).toFixed(2) }) + extra,
          battery.low ? "bad" : "",
        ),
      );
    }
    if (sensors?.ambient) {
      parts.push(chip(`${Number(sensors.ambient.temp_c).toFixed(1)} °C · ${Math.round(sensors.ambient.rh_pct)}%`));
    }
    if (frame.panel) parts.push(chip(`${frame.panel.width}×${frame.panel.height}`));
    if (frame.mdns) parts.push(chip(t("frames.found")));
    if (frame.manual) parts.push(chip(t("frames.manual")));
    if (frame.error) parts.push(chip(frame.error, "bad"));
    chips.replaceChildren(...parts);

    // The gallery and storing need this firmware's portal; showing needs only
    // OpenDisplay and a known panel.
    galleryToggle.disabled = !frame.portal;
    store.disabled = !packed || !frame.portal;
    show.disabled = !packed || !frame.panel;
    noteShow.disabled = !frame.panel;
    screenShow.disabled = !frame.panel;
    actions.replaceChildren(galleryToggle, ...(frame.manual ? [remove] : []));
    send.hidden = !frame.panel && !frame.portal;
  }

  return { element, update };
}

$("frames-scan").addEventListener("click", () => loadFrames(true));
$("add-frame").addEventListener("click", async () => {
  const request = {
    host: $("add-host").value.trim(),
    name: $("add-name").value.trim(),
    password: $("add-password").value,
  };
  if (!request.host) return;
  say("frames-message", t("frames.adding"), "");
  const answer = await post("/api/frames/add", request);
  if (answer.ok) {
    renderFrames((await answer.json()).frames);
    say("frames-message", t("frames.added", { host: request.host }), "ok");
    $("add-host").value = $("add-name").value = $("add-password").value = "";
  } else {
    say("frames-message", (await answer.text()).trim(), "bad");
  }
});
setInterval(() => {
  if (!$("tab-frames").hidden && document.visibilityState === "visible") loadFrames();
}, 30000);

// ---- plugins ---------------------------------------------------------------

/// What the manifests call themselves, filled by loadPlugins so the job list can
/// name a plugin in the interface's language: a job stores the title its plugin
/// had when it was saved, which is a snapshot, not a translation.
const pluginTitles = new Map();

/// A manifest carries its words in the interface's language under `i18n.<lang>`
/// (`bridge/plugins/README.md`). Without a translation the authored text stands:
/// a third-party plugin is at worst in its author's language, never blank.
const localized = (object, field) => object?.i18n?.[language]?.[field] || object?.[field] || "";

/// Which plugin is expanded. Kept outside the DOM because loadPlugins rebuilds
/// every card — on a scan, on a language switch — and an accordion that forgets
/// what it had open is worse than no accordion.
let openPlugin = null;

/// The groups of the Plugins tab, in the order it shows them: what a frame
/// shows every day first, examples last. The labels are the interface's own
/// (`category.<id>`), because the group is the interface's idea and not a
/// plugin's: a manifest names one, it does not name it in words.
const PLUGIN_GROUPS = ["weather", "home", "news", "pictures", "examples"];

/// A group's heading word. The page knows its own groups; one that a manifest
/// invented keeps the name the author wrote, which is at worst untranslated.
const groupName = (id) =>
  id === "other" || PLUGIN_GROUPS.includes(id) ? t(`category.${id}`) : id;

/// The plugins of one group, each group's plugins sorted by the title the page
/// shows them under. A group the page does not know (a plugin author's own)
/// follows the known ones under its own name; a plugin that names no group is
/// "other" and comes last.
function arrangePlugins(plugins) {
  const place = (id) => {
    const known = PLUGIN_GROUPS.indexOf(id);
    return known >= 0 ? known : id === "other" ? PLUGIN_GROUPS.length + 1 : PLUGIN_GROUPS.length;
  };
  const groups = new Map();
  for (const plugin of plugins) {
    const id = plugin.category || "other";
    if (!groups.has(id)) groups.set(id, []);
    groups.get(id).push(plugin);
  }
  const label = (plugin) => localized(plugin, "title");
  return [...groups.entries()]
    .sort(([a], [b]) => place(a) - place(b) || a.localeCompare(b))
    .map(([id, list]) => [id, list.sort((a, b) => label(a).localeCompare(label(b), language))]);
}

async function loadPlugins() {
  try {
    const [pluginAnswer, frameAnswer] = await Promise.all([fetch("/api/plugins"), fetch("/api/frames")]);
    if (!pluginAnswer.ok) throw new Error((await pluginAnswer.text()).trim());
    const { plugins, problems, dirs } = await pluginAnswer.json();
    const { frames } = frameAnswer.ok ? await frameAnswer.json() : { frames: [] };
    // The jump selector is the short way in when a dozen closed sections are
    // all on screen; it follows the accordion rather than duplicating its state.
    pluginTitles.clear();
    for (const plugin of plugins) pluginTitles.set(plugin.name, localized(plugin, "title"));
    const arranged = arrangePlugins(plugins);
    // The selector carries the same groups, so the list and the jump agree.
    const jump = $("plugin-jump");
    jump.replaceChildren(new Option(t("plugins.jump"), ""));
    for (const [id, list] of arranged) {
      const group = document.createElement("optgroup");
      group.label = groupName(id);
      for (const plugin of list) group.append(new Option(pluginTitles.get(plugin.name), plugin.name));
      jump.append(group);
    }
    jump.value = plugins.some((plugin) => plugin.name === openPlugin) ? openPlugin : "";
    $("plugin-jump-row").hidden = plugins.length === 0;
    $("plugin-list").replaceChildren(
      ...(plugins.length
        ? arranged.map(([id, list]) => pluginGroup(id, list, frames, dirs))
        : [text("p", t("plugins.empty", { dirs: dirs.join(", ") }), "empty")]),
    );
    $("plugin-problems").textContent = problems.length
      ? t("plugins.skipped", { problems: problems.join("; ") })
      : "";
  } catch (error) {
    $("plugin-list").replaceChildren(text("p", t("load.failed", { error: error.message }), "empty"));
  }
}

/// One group of the Plugins tab: a heading and its closed sections.
function pluginGroup(id, plugins, frames, roots) {
  const section = document.createElement("section");
  section.className = "plugin-group";
  section.append(
    text("h3", groupName(id)),
    ...plugins.map((plugin) => pluginCard(plugin, frames, roots)),
  );
  return section;
}

/// A plugin's directory as the page names it: relative to the plugins directory
/// that holds it, which is named by the directory above it. A built-in plugin
/// therefore reads `bridge/air` and one of your own `easellet-bridge/…`, where
/// the full path would run off the end of the row and say nothing more.
function shortDir(dir, roots) {
  const root = roots
    .filter((candidate) => dir === candidate || dir.startsWith(`${candidate}/`))
    .sort((a, b) => b.length - a.length)[0];
  if (!root) return dir;
  const name = root.split("/").filter(Boolean).slice(-2, -1)[0] ?? root;
  const rest = dir.slice(root.length).replace(/^\//, "");
  return rest ? `${name}/${rest}` : name;
}

function pluginCard(plugin, frames, roots) {
  // A closed section: eight plugins' worth of forms is a wall of fields, and
  // the one being used is the one that should be on screen.
  const element = document.createElement("details");
  element.className = "plugin";
  element.id = `plugin-${plugin.name}`;
  const summary = document.createElement("summary");
  summary.append(
    text("span", localized(plugin, "title"), "title"),
    text("span", shortDir(plugin.dir, roots), "where"),
  );
  const inner = document.createElement("div");
  inner.className = "plugin-inner";
  const description = localized(plugin, "description");
  if (description) inner.append(text("p", description, "note"));

  const form = document.createElement("div");
  form.className = "form";
  const inputs = {};
  for (const param of (plugin.param ?? []).filter((param) => !param.hidden)) {
    const box = document.createElement("div");
    box.className = "field";
    const label = text("label", localized(param, "title") + (param.required ? " *" : ""));
    let input;
    if (param.type === "choice") {
      input = document.createElement("select");
      for (const choice of param.choices) input.append(new Option(choice, choice));
      if (param.default != null) input.value = param.default;
    } else if (param.type === "bool") {
      input = document.createElement("input");
      input.type = "checkbox";
      input.checked = param.default === true;
    } else if (param.type === "text") {
      input = document.createElement("textarea");
      input.value = param.default ?? "";
    } else {
      input = document.createElement("input");
      input.type = param.type === "secret" ? "password" : param.type === "number" ? "number" : "text";
      input.value = param.default ?? "";
    }
    input.id = `plugin-${plugin.name}-${param.id}`;
    label.htmlFor = input.id;
    inputs[param.id] = input;
    box.append(label, input);
    const note = localized(param, "note");
    if (note) box.append(text("p", note, "note"));
    form.append(box);
  }
  const target = document.createElement("select");
  target.append(new Option(t("plugin.targetNone"), ""));
  for (const frame of frames) target.append(new Option(`${frame.name} (${frame.host})`, frame.id));
  const preview = text("button", t("plugin.preview"));
  const show = text("button", t("plugin.show"), "primary");
  const buttons = document.createElement("div");
  buttons.className = "row";
  buttons.append(target, preview, show);
  const message = text("p", "", "message");
  form.append(buttons, message);
  const image = document.createElement("img");
  image.className = "preview";
  image.alt = t("plugin.alt");
  image.hidden = true;
  const body = document.createElement("div");
  body.className = "body";
  body.append(form, image);

  const values = () => {
    const params = {};
    for (const [id, input] of Object.entries(inputs)) {
      params[id] = input.type === "checkbox" ? input.checked : input.value;
    }
    return { params, frame: target.value };
  };
  const busy = (on) => {
    preview.disabled = on;
    show.disabled = on || !target.value;
  };
  busy(false);
  target.addEventListener("change", () => busy(false));
  preview.addEventListener("click", async () => {
    busy(true);
    message.textContent = t("plugin.running");
    message.className = "message";
    try {
      const answer = await post(`/api/plugins/${plugin.name}/preview`, values());
      if (answer.ok && answer.headers.get("Content-Type") === "image/png") {
        if (image.src) URL.revokeObjectURL(image.src);
        image.src = URL.createObjectURL(await answer.blob());
        image.hidden = false;
        message.textContent = t("plugin.done", { ms: answer.headers.get("X-Plugin-Ms") ?? "?" });
        message.className = "message ok";
      } else {
        message.textContent = (await answer.text()).trim();
        message.className = answer.ok ? "message" : "message bad";
      }
    } finally {
      busy(false);
    }
  });
  show.addEventListener("click", async () => {
    busy(true);
    message.textContent = t("plugin.showing");
    message.className = "message";
    try {
      const answer = await post(`/api/plugins/${plugin.name}/show`, values());
      message.textContent = (await answer.text()).trim();
      message.className = answer.ok ? "message ok" : "message bad";
    } finally {
      busy(false);
    }
  });
  // Save the form as a job: which frames, how often, how.
  const schedule = document.createElement("details");
  schedule.className = "schedule";
  schedule.append(text("summary", t("jobs.scheduleSave")));
  const pick = document.createElement("div");
  pick.className = "frames-pick";
  const picks = frames.map((frame) => {
    const label = document.createElement("label");
    label.className = "option";
    const box = document.createElement("input");
    box.type = "checkbox";
    box.value = frame.id;
    label.append(box, ` ${frame.name}`);
    pick.append(label);
    return box;
  });
  if (!frames.length) pick.append(text("span", t("jobs.noFrames"), "note"));
  const every = document.createElement("select");
  for (const [minutes, key] of [[0, "manual"], [15, "15"], [30, "30"], [60, "hour"], [180, "3h"], [360, "6h"], [720, "12h"], [1440, "day"]]) {
    every.append(new Option(t(`jobs.every.${key}`), String(minutes)));
  }
  every.value = String({ "15m": 15, "30m": 30, "1h": 60, "3h": 180, "6h": 360, "12h": 720, "24h": 1440 }[plugin.default_every] ?? 60);
  const mode = document.createElement("select");
  mode.append(new Option(t("jobs.mode.show"), "show"), new Option(t("jobs.mode.gallery"), "gallery"));
  const priority = document.createElement("select");
  for (let level = 0; level <= 9; level += 1) {
    priority.append(new Option(t("jobs.priority", { level }), String(level)));
  }
  const saveJob = text("button", t("jobs.save"), "primary");
  const scheduleRow = document.createElement("div");
  scheduleRow.className = "row";
  scheduleRow.append(every, mode, priority, saveJob);
  const scheduleMessage = text("p", "", "message");
  schedule.append(pick, scheduleRow, scheduleMessage);
  saveJob.addEventListener("click", async () => {
    const chosen = picks.filter((box) => box.checked).map((box) => box.value);
    const request = {
      plugin: plugin.name,
      params: values().params,
      frames: chosen,
      every_min: Number(every.value),
      mode: mode.value,
      priority: Number(priority.value),
    };
    const answer = await post("/api/jobs/save", request);
    scheduleMessage.textContent = answer.ok
      ? t("jobs.saved", { id: (await answer.json()).id })
      : (await answer.text()).trim();
    scheduleMessage.className = answer.ok ? "message ok" : "message bad";
    if (answer.ok) loadJobs();
  });
  form.append(schedule);

  // One open at a time: the point is that the page stays short enough to see
  // the next plugin without scrolling past this one.
  element.addEventListener("toggle", () => {
    if (element.open) {
      openPlugin = plugin.name;
      for (const other of $("plugin-list").querySelectorAll("details.plugin")) {
        if (other !== element) other.open = false;
      }
    } else if (openPlugin === plugin.name) {
      openPlugin = null;
    }
    const jump = $("plugin-jump");
    if (jump.value !== (openPlugin ?? "")) jump.value = openPlugin ?? "";
  });
  inner.append(body);

  element.append(summary, inner);
  element.open = plugin.name === openPlugin;
  return element;
}

/// Result words the bridge reports (`shown`, `queued`, …), translated at render
/// time: the keys are the protocol's, not the interface's.
const results = (result) => t(`results.${result}`);

async function loadJobs() {
  try {
    const [jobAnswer, frameAnswer] = await Promise.all([fetch("/api/jobs"), fetch("/api/frames")]);
    if (!jobAnswer.ok) throw new Error((await jobAnswer.text()).trim());
    const { jobs, runs } = await jobAnswer.json();
    const { frames } = frameAnswer.ok ? await frameAnswer.json() : { frames: [] };
    const nameOf = (id) => frames.find((frame) => frame.id === id)?.name ?? id;
    $("job-list").replaceChildren(
      ...(jobs.length
        ? jobs.map((job) => {
            const item = document.createElement("article");
            item.className = "turn";
            const meta = document.createElement("div");
            meta.className = "meta";
            for (const part of [
              job.every_min ? t("jobs.everyN", { n: job.every_min }) : t("jobs.manual"),
              job.mode === "gallery" ? t("jobs.modeGallery") : t("jobs.modeShow"),
              t("jobs.priority", { level: job.priority }),
              job.enabled ? "" : t("jobs.disabled"),
            ]) if (part) meta.append(text("span", part));
            const params = Object.entries(job.params)
              .map(([key, value]) => `${key}=${value === true ? "•••" : value}`)
              .join(", ");
            const row = document.createElement("div");
            row.className = "row";
            const runIt = text("button", t("jobs.run"));
            const remove = text("button", t("jobs.delete"), "danger");
            const message = text("p", "", "message");
            runIt.addEventListener("click", async () => {
              runIt.disabled = true;
              message.textContent = t("jobs.running");
              const answer = await post("/api/jobs/run", { id: job.id });
              if (answer.ok) {
                const { runs: done } = await answer.json();
                message.textContent = done
                  .map(
                    (run) =>
                      `${run.frame}: ${results(run.result)}${run.message ? ` — ${run.message}` : ""}`,
                  )
                  .join("\n");
                message.className = "message ok";
              } else {
                message.textContent = (await answer.text()).trim();
                message.className = "message bad";
              }
              runIt.disabled = false;
              loadJobs();
            });
            remove.addEventListener("click", async () => {
              if (!remove.classList.contains("armed")) {
                remove.classList.add("armed");
                remove.textContent = t("jobs.confirm");
                return;
              }
              await post("/api/jobs/delete", { id: job.id });
              loadJobs();
            });
            row.append(runIt, remove);
            item.append(
              meta,
              text(
                "div",
                `${pluginTitles.get(job.plugin) || job.title || job.plugin} → ${job.frames.map(nameOf).join(", ")}`,
                "heard",
              ),
              text("div", params || t("jobs.noParams"), "answer"),
              row,
              message,
            );
            return item;
          })
        : [text("p", t("jobs.empty"), "empty")]),
    );
    $("run-list").replaceChildren(
      ...(runs.length
        ? runs.map((run) => {
            const item = document.createElement("article");
            item.className = run.result === "failed" ? "turn rejected" : "turn";
            const meta = document.createElement("div");
            meta.className = "meta";
            for (const part of [
              new Date(run.at_ms).toLocaleString(),
              run.plugin,
              run.frame,
              t("runs.ms", { ms: run.ms }),
            ]) {
              meta.append(text("span", part));
            }
            item.append(
              meta,
              text("div", results(run.result), run.result === "failed" ? "why" : "heard"),
            );
            if (run.message) item.append(text("div", run.message, "answer"));
            return item;
          })
        : [text("p", t("runs.empty"), "empty")]),
    );
  } catch (error) {
    $("job-list").replaceChildren(text("p", t("load.failed", { error: error.message }), "empty"));
  }
}

// ---- assistant history -----------------------------------------------------

/// Trigger names the frame sends (`key`, `voice`, …), translated at render time.
const trigger = (name) => t(`history.trigger.${name || "text"}`);
async function loadHistory() {
  try {
    const answer = await fetch("/api/history?limit=100");
    if (!answer.ok) throw new Error(await answer.text());
    const { turns } = await answer.json();
    renderTurns(turns);
  } catch (error) {
    $("turns").replaceChildren(text("p", t("load.failed", { error: error.message }), "empty"));
  }
}

function text(tag, content, className) {
  const element = document.createElement(tag);
  element.textContent = content;
  if (className) element.className = className;
  return element;
}

function renderTurns(turns) {
  $("history-count").textContent = tCount("history.count", turns.length);
  if (!turns.length) {
    $("turns").replaceChildren(text("p", t("history.empty"), "empty"));
    return;
  }
  $("turns").replaceChildren(
    ...turns.map((turn) => {
      const item = document.createElement("article");
      item.className = "turn";
      const when = new Date(turn.at_ms).toLocaleString();
      const timings = [
        turn.stt_ms != null && t("history.stt", { ms: turn.stt_ms }),
        turn.llm_ms != null && t("history.llm", { ms: turn.llm_ms }),
        turn.tts_ms != null && t("history.tts", { ms: turn.tts_ms }),
      ].filter(Boolean);
      const meta = document.createElement("div");
      meta.className = "meta";
      for (const part of [when, turn.peer, trigger(turn.trigger), ...timings]) {
        if (part) meta.append(text("span", part));
      }
      item.append(meta);
      if (turn.heard != null) item.append(text("div", turn.heard || t("history.emptyTurn"), "heard"));
      if (turn.answer) item.append(text("div", turn.answer, "answer"));
      if (turn.error) item.append(text("div", turn.error, "why"));
      return item;
    }),
  );
}

async function ask() {
  const question = $("ask-text").value.trim();
  if (!question) return;
  $("ask-send").disabled = true;
  say("ask-answer", t("assistant.thinking"), "");
  $("ask-audio").hidden = true;
  try {
    const answer = await post("/api/assistant/ask", { text: question });
    if (!answer.ok) throw new Error((await answer.text()).trim());
    const { answer: reply, audio } = await answer.json();
    say("ask-answer", reply, "ok");
    if (audio) {
      $("ask-audio").src = audio;
      $("ask-audio").hidden = false;
      $("ask-audio").play().catch(() => {});
    }
    loadHistory();
  } catch (error) {
    say("ask-answer", error.message, "bad");
  } finally {
    $("ask-send").disabled = false;
  }
}
$("ask-send").addEventListener("click", ask);
$("ask-text").addEventListener("keydown", (event) => {
  if (event.key === "Enter") ask();
});

$("history-refresh").addEventListener("click", loadHistory);
setInterval(() => {
  if (!$("tab-assistant").hidden && document.visibilityState === "visible") loadHistory();
}, 5000);

// ---- settings --------------------------------------------------------------

// How each key is edited, in the order the form shows them.
// Titles and notes come from STRINGS (`field.<key>.title` / `.note`) so the
// form is translated with the rest of the page; the structure here is the form.
const FIELDS = [
  { key: "wake", kind: "list" },
  { key: "llm_system", kind: "textarea" },
  { key: "stt_prompt", kind: "textarea" },
  { key: "llm_url", kind: "text" },
  { key: "token", kind: "secret" },
  { key: "ui_password", kind: "secret" },
  { key: "ui_hosts", kind: "list" },
  { key: "firmware_repo", kind: "text" },
  { key: "firmware_channel", kind: "text" },
  { key: "firmware_token", kind: "secret" },
  { key: "firmware_check_h", kind: "number" },
  { key: "firmware_index_url", kind: "text" },
  { key: "addr", kind: "text" },
  { key: "stt", kind: "text" },
  { key: "llm", kind: "text" },
  { key: "tts", kind: "text" },
];

let loaded = null;

async function loadSettings() {
  try {
    const answer = await fetch("/api/settings");
    if (!answer.ok) throw new Error(await answer.text());
    loaded = await answer.json();
    renderSettings(loaded);
    $("settings-file").textContent = t("settings.file", { file: loaded.file });
    say("settings-message", "", "");
  } catch (error) {
    say("settings-message", t("load.failed", { error: error.message }), "bad");
  }
}

function renderSettings({ values, pinned, restart }) {
  const form = $("settings-form");
  form.replaceChildren(
    ...FIELDS.map((field) => {
      const box = document.createElement("div");
      box.className = "field";
      const label = text("label", t(`field.${field.key}.title`));
      label.htmlFor = `setting-${field.key}`;
      if (restart.includes(field.key)) label.append(text("span", t("settings.restartBadge"), "badge"));
      if (pinned.includes(field.key)) label.append(text("span", t("settings.pinnedBadge"), "badge warn"));
      let input;
      if (field.kind === "textarea") {
        input = document.createElement("textarea");
        input.value = values[field.key] ?? "";
      } else {
        input = document.createElement("input");
        input.type =
          field.kind === "secret" ? "password" : field.kind === "number" ? "number" : "text";
        if (field.kind === "secret") {
          // The value never comes back from the bridge: empty keeps it, "-" clears.
          input.placeholder = values[field.key] ? t("settings.secretSet") : t("settings.secretUnset");
        } else if (field.kind === "list") {
          input.value = (values[field.key] ?? []).join(", ");
        } else {
          input.value = values[field.key] ?? "";
        }
      }
      input.id = `setting-${field.key}`;
      input.dataset.key = field.key;
      input.disabled = pinned.includes(field.key);
      box.append(label, input);
      const note = t(`field.${field.key}.note`);
      if (note !== `field.${field.key}.note`) box.append(text("p", note, "note"));
      return box;
    }),
  );
}

function changes() {
  const change = {};
  for (const field of FIELDS) {
    const input = $(`setting-${field.key}`);
    if (!input || input.disabled) continue;
    const before = loaded.values[field.key];
    if (field.kind === "secret") {
      if (input.value) change[field.key] = input.value;
    } else if (field.kind === "list") {
      if (input.value !== (before ?? []).join(", ")) change[field.key] = input.value;
    } else if (field.kind === "number") {
      // The settings file holds a number: the API refuses text for this key.
      if (input.value !== String(before ?? "")) change[field.key] = Number(input.value);
    } else if (input.value !== (before ?? "")) {
      change[field.key] = input.value;
    }
  }
  return change;
}

$("settings-save").addEventListener("click", async () => {
  const change = changes();
  if (!Object.keys(change).length) {
    say("settings-message", t("settings.nothingToSave"), "");
    return;
  }
  try {
    const answer = await post("/api/settings", change);
    const body = await answer.text();
    if (!answer.ok) throw new Error(body.trim());
    const { changed, restart } = JSON.parse(body);
    const after = restart.length ? t("settings.restartHint", { list: restart.join(", ") }) : "";
    say(
      "settings-message",
      t("settings.saved", { list: changed.join(", ") || t("settings.nothing"), after }),
      "ok",
    );
    await loadSettings();
    if (restart.length) say("settings-message", t("settings.savedShort", { after }), "ok");
    loadStatus();
  } catch (error) {
    say("settings-message", error.message, "bad");
  }
});
$("settings-reload").addEventListener("click", loadSettings);

// ---- start -----------------------------------------------------------------

// The cards, the job list and the settings form are built nodes, not markup, so
// a language switch rebuilds them from the same API instead of patching text:
// the card map is dropped and the current tab reloads itself.
addEventListener("easellet-language", () => {
  cards.clear();
  loadStatus();
  showTab();
});

// Jump to a plugin: open it, then bring it to the top of the screen. The select
// then keeps showing which section is open, so it doubles as a state indicator.
$("plugin-jump").addEventListener("change", (event) => {
  const name = event.target.value;
  if (!name) return;
  const card = document.getElementById(`plugin-${name}`);
  if (!card) return;
  card.open = true;
  card.scrollIntoView({ behavior: "smooth", block: "start" });
});

loadStatus();
setInterval(loadStatus, 30000);
showTab();
// Enter in a field must not submit the form and reload the page.
$("settings-form").addEventListener("submit", (event) => event.preventDefault());
