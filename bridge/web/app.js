// The bridge's web UI. Plain ES module, no build step, like the frame's page.

// The frame portal's own converter, served by the bridge from tools/pf6.mjs.
import { WIDTH, HEIGHT, FRAME_BYTES, packImage, drawFrame } from "./pf6.mjs";

const $ = (id) => document.getElementById(id);

// Every POST carries this header; the bridge refuses a POST without it, which is
// what keeps another site's page from changing settings through your browser.
function post(url, body) {
  return fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json", "X-Photoframe": "1" },
    body: JSON.stringify(body),
  });
}

// A raw picture (or an empty body) goes through the same header.
function postBytes(url, bytes) {
  return fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/octet-stream", "X-Photoframe": "1" },
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
    loadPlugins();
    loadJobs();
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
    $("link-text").textContent = `на связи · ${uptime(status.uptime_s)}`;
    $("backends-chip").textContent =
      `${status.backends.stt} · ${short(status.backends.llm)} · ${short(status.backends.tts)}`;
    $("version").textContent = status.version;
  } catch {
    $("link-dot").className = "dot off";
    $("link-text").textContent = "нет связи";
  }
}

function short(spec) {
  // "cmd:bridge/scripts/llm-local.py" -> "llm-local.py"
  return spec.startsWith("cmd:") ? spec.slice(4).split(/\s/)[0].split("/").pop() : spec;
}

function uptime(seconds) {
  if (seconds < 120) return `${seconds} с`;
  if (seconds < 7200) return `${Math.round(seconds / 60)} мин`;
  return `${Math.round(seconds / 3600)} ч`;
}

// ---- frames ----------------------------------------------------------------

async function loadFrames(scan = false) {
  $("frames-note").textContent = scan ? "ищу…" : "";
  try {
    const answer = scan ? await postBytes("/api/frames/scan") : await fetch("/api/frames");
    if (!answer.ok) throw new Error((await answer.text()).trim());
    const { frames } = await answer.json();
    renderFrames(frames);
    $("frames-note").textContent = `${frames.length} ${frames.length === 1 ? "рамка" : "рамок"}`;
  } catch (error) {
    say("frames-message", `Не удалось: ${error.message}`, "bad");
    $("frames-note").textContent = "";
  }
}

function ago(seconds) {
  if (seconds == null) return "ещё не отвечала";
  if (seconds < 90) return "на связи";
  if (seconds < 5400) return `видели ${Math.round(seconds / 60)} мин назад`;
  return `видели ${Math.round(seconds / 3600)} ч назад`;
}

// Frame cards are rebuilt on every scan, but an open gallery or a prepared
// picture must survive that; so cards are kept by id and only their header redrawn.
const cards = new Map();

function renderFrames(frames) {
  const list = $("frame-list");
  if (!frames.length) {
    list.replaceChildren(text("p", "Рамок не найдено. Рамка на батарее спит и не отвечает; разбудите её кнопкой или добавьте по адресу.", "empty"));
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
  mode.append(new Option("заполнить кадр", "cover"), new Option("целиком", "contain"));
  const dither = document.createElement("label");
  dither.className = "option";
  const ditherBox = document.createElement("input");
  ditherBox.type = "checkbox";
  ditherBox.checked = true;
  dither.append(ditherBox, " плавно");
  const show = text("button", "Показать сейчас", "primary");
  const store = text("button", "Сохранить в галерею");
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
      message.textContent = `Не картинка: ${error.message}`;
      message.className = "message bad";
    }
  });
  mode.addEventListener("change", prepare);
  ditherBox.addEventListener("change", prepare);

  async function deliver(action, button) {
    if (!packed) return;
    button.disabled = true;
    message.textContent = action === "display" ? "Отправляю…" : "Сохраняю…";
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
  const galleryToggle = text("button", "Галерея");
  const remove = text("button", "Убрать из списка", "danger");
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
    gallery.replaceChildren(text("p", "Загружаю…", "empty"));
    try {
      const answer = await fetch(`/api/frames/${frame.id}/gallery`);
      if (!answer.ok) throw new Error((await answer.text()).trim());
      const { images } = await answer.json();
      if (!images.length) {
        gallery.replaceChildren(text("p", "В галерее пусто.", "empty"));
        return;
      }
      gallery.replaceChildren(...images.map(thumbnail));
    } catch (error) {
      gallery.replaceChildren(text("p", `Галерея недоступна: ${error.message}`, "empty"));
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
    const showIt = text("button", "Показать");
    const deleteIt = text("button", "Удалить", "danger");
    // The frame refuses deletion outside its writable directory anyway; the
    // button is only offered where it would succeed.
    deleteIt.hidden = !image.name.toUpperCase().startsWith("PFTEST/");
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
        deleteIt.textContent = "Точно?";
        setTimeout(() => {
          deleteIt.classList.remove("armed");
          deleteIt.textContent = "Удалить";
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
  const noteSummary = text("summary", "Сообщение на рамку");
  const noteTitle = document.createElement("input");
  noteTitle.type = "text";
  noteTitle.placeholder = "Заголовок";
  const noteBody = document.createElement("textarea");
  noteBody.placeholder = "Текст сообщения";
  const noteAccent = document.createElement("select");
  for (const [value, label] of [["blue", "синий"], ["red", "красный"], ["green", "зелёный"], ["yellow", "жёлтый"], ["black", "чёрный"]]) {
    noteAccent.append(new Option(label, value));
  }
  const notePreview = text("button", "Предпросмотр");
  const noteShow = text("button", "Показать на рамке", "primary");
  const noteImage = document.createElement("img");
  noteImage.className = "preview";
  noteImage.alt = "Как это покажет рамка";
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
    title: noteTitle.value.trim() || "Сообщение",
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
    noteMessage.textContent = "Отправляю…";
    noteMessage.className = "message";
    try {
      const answer = await post(`/api/frames/${frame.id}/card`, card());
      noteMessage.textContent = (await answer.text()).trim();
      noteMessage.className = answer.ok ? "message ok" : "message bad";
    } finally {
      noteShow.disabled = !frame.panel;
    }
  });

  const actions = document.createElement("div");
  actions.className = "row";
  element.append(header, chips, actions, send, note, gallery);

  function update(next) {
    frame = next;
    const title = document.createElement("div");
    title.append(text("h3", frame.name), text("div", `${frame.host} · ${frame.mac ?? "без MAC"}${frame.firmware ? ` · fw ${frame.firmware}` : ""}`, "where"));
    const state = document.createElement("div");
    state.className = "chips";
    const dot = document.createElement("span");
    dot.className = frame.online ? "dot on" : "dot off";
    const link = chip("", "");
    link.append(dot, ` ${frame.online ? "на связи" : ago(frame.seen_s)}`);
    state.append(link);
    header.replaceChildren(title, state);

    const parts = [];
    const sensors = frame.sensors;
    if (sensors?.battery?.present) {
      const battery = sensors.battery;
      const extra = battery.charging ? " · заряжается" : battery.usb ? " · от USB" : "";
      parts.push(chip(`батарея ${battery.percent}% (${(battery.mv / 1000).toFixed(2)} В)${extra}`, battery.low ? "bad" : ""));
    }
    if (sensors?.ambient) {
      parts.push(chip(`${Number(sensors.ambient.temp_c).toFixed(1)} °C · ${Math.round(sensors.ambient.rh_pct)}%`));
    }
    if (frame.panel) parts.push(chip(`${frame.panel.width}×${frame.panel.height}`));
    if (frame.mdns) parts.push(chip("найдена в сети"));
    if (frame.manual) parts.push(chip("добавлена вручную"));
    if (frame.error) parts.push(chip(frame.error, "bad"));
    chips.replaceChildren(...parts);

    // The gallery and storing need this firmware's portal; showing needs only
    // OpenDisplay and a known panel.
    galleryToggle.disabled = !frame.portal;
    store.disabled = !packed || !frame.portal;
    show.disabled = !packed || !frame.panel;
    noteShow.disabled = !frame.panel;
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
  say("frames-message", "Добавляю и проверяю…", "");
  const answer = await post("/api/frames/add", request);
  if (answer.ok) {
    renderFrames((await answer.json()).frames);
    say("frames-message", `Добавлена: ${request.host}`, "ok");
    $("add-host").value = $("add-name").value = $("add-password").value = "";
  } else {
    say("frames-message", (await answer.text()).trim(), "bad");
  }
});
setInterval(() => {
  if (!$("tab-frames").hidden && document.visibilityState === "visible") loadFrames();
}, 30000);

// ---- plugins ---------------------------------------------------------------

async function loadPlugins() {
  try {
    const [pluginAnswer, frameAnswer] = await Promise.all([fetch("/api/plugins"), fetch("/api/frames")]);
    if (!pluginAnswer.ok) throw new Error((await pluginAnswer.text()).trim());
    const { plugins, problems, dirs } = await pluginAnswer.json();
    const { frames } = frameAnswer.ok ? await frameAnswer.json() : { frames: [] };
    $("plugin-list").replaceChildren(
      ...(plugins.length
        ? plugins.map((plugin) => pluginCard(plugin, frames))
        : [text("p", `Плагинов нет. Каталоги: ${dirs.join(", ")}`, "empty")]),
    );
    $("plugin-problems").textContent = problems.length ? `Пропущены: ${problems.join("; ")}` : "";
  } catch (error) {
    $("plugin-list").replaceChildren(text("p", `Не удалось загрузить: ${error.message}`, "empty"));
  }
}

function pluginCard(plugin, frames) {
  const element = document.createElement("article");
  element.className = "plugin";
  const head = document.createElement("div");
  head.append(text("h3", plugin.title), text("div", `${plugin.name} · ${plugin.dir}`, "where"));
  if (plugin.description) head.append(text("p", plugin.description, "note"));

  const form = document.createElement("div");
  form.className = "form";
  const inputs = {};
  for (const param of (plugin.param ?? []).filter((param) => !param.hidden)) {
    const box = document.createElement("div");
    box.className = "field";
    const label = text("label", param.title + (param.required ? " *" : ""));
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
    if (param.note) box.append(text("p", param.note, "note"));
    form.append(box);
  }
  const target = document.createElement("select");
  target.append(new Option("без рамки (только посмотреть)", ""));
  for (const frame of frames) target.append(new Option(`${frame.name} (${frame.host})`, frame.id));
  const preview = text("button", "Предпросмотр");
  const show = text("button", "Показать на рамке", "primary");
  const buttons = document.createElement("div");
  buttons.className = "row";
  buttons.append(target, preview, show);
  const message = text("p", "", "message");
  form.append(buttons, message);
  const image = document.createElement("img");
  image.className = "preview";
  image.alt = "Как это покажет рамка";
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
    message.textContent = "Запускаю…";
    message.className = "message";
    try {
      const answer = await post(`/api/plugins/${plugin.name}/preview`, values());
      if (answer.ok && answer.headers.get("Content-Type") === "image/png") {
        if (image.src) URL.revokeObjectURL(image.src);
        image.src = URL.createObjectURL(await answer.blob());
        image.hidden = false;
        message.textContent = `Готово за ${answer.headers.get("X-Plugin-Ms") ?? "?"} мс`;
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
    message.textContent = "Запускаю и отправляю…";
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
  schedule.append(text("summary", "Сохранить как задание"));
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
  if (!frames.length) pick.append(text("span", "рамок пока нет", "note"));
  const every = document.createElement("select");
  for (const [minutes, label] of [[0, "только вручную"], [15, "каждые 15 мин"], [30, "каждые 30 мин"], [60, "каждый час"], [180, "каждые 3 ч"], [360, "каждые 6 ч"], [720, "каждые 12 ч"], [1440, "раз в сутки"]]) {
    every.append(new Option(label, String(minutes)));
  }
  every.value = String({ "15m": 15, "30m": 30, "1h": 60, "3h": 180, "6h": 360, "12h": 720, "24h": 1440 }[plugin.default_every] ?? 60);
  const mode = document.createElement("select");
  mode.append(new Option("показывать (не сохранять на рамке)", "show"), new Option("добавлять в галерею рамки", "gallery"));
  const priority = document.createElement("select");
  for (let level = 0; level <= 9; level += 1) priority.append(new Option(`приоритет ${level}`, String(level)));
  const saveJob = text("button", "Сохранить задание", "primary");
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
    scheduleMessage.textContent = answer.ok ? `Сохранено: ${(await answer.json()).id}` : (await answer.text()).trim();
    scheduleMessage.className = answer.ok ? "message ok" : "message bad";
    if (answer.ok) loadJobs();
  });
  form.append(schedule);

  element.append(head, body);
  return element;
}

const RESULTS = {
  shown: "показано", queued: "ждёт рамку", stored: "в галерее", unchanged: "без изменений",
  "nothing new": "ничего нового", failed: "ошибка",
};

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
              job.every_min ? `каждые ${job.every_min} мин` : "вручную",
              job.mode === "gallery" ? "в галерею" : "показ",
              `приоритет ${job.priority}`,
              job.enabled ? "" : "выключено",
            ]) if (part) meta.append(text("span", part));
            const params = Object.entries(job.params)
              .map(([key, value]) => `${key}=${value === true ? "•••" : value}`)
              .join(", ");
            const row = document.createElement("div");
            row.className = "row";
            const runIt = text("button", "Запустить сейчас");
            const remove = text("button", "Удалить", "danger");
            const message = text("p", "", "message");
            runIt.addEventListener("click", async () => {
              runIt.disabled = true;
              message.textContent = "Запускаю…";
              const answer = await post("/api/jobs/run", { id: job.id });
              if (answer.ok) {
                const { runs: done } = await answer.json();
                message.textContent = done.map((run) => `${run.frame}: ${RESULTS[run.result] ?? run.result}${run.message ? ` — ${run.message}` : ""}`).join("\n");
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
                remove.textContent = "Точно?";
                return;
              }
              await post("/api/jobs/delete", { id: job.id });
              loadJobs();
            });
            row.append(runIt, remove);
            item.append(
              meta,
              text("div", `${job.title || job.plugin} → ${job.frames.map(nameOf).join(", ")}`, "heard"),
              text("div", params || "без параметров", "answer"),
              row,
              message,
            );
            return item;
          })
        : [text("p", "Заданий пока нет: настройте плагин выше и сохраните его как задание.", "empty")]),
    );
    $("run-list").replaceChildren(
      ...(runs.length
        ? runs.map((run) => {
            const item = document.createElement("article");
            item.className = run.result === "failed" ? "turn rejected" : "turn";
            const meta = document.createElement("div");
            meta.className = "meta";
            for (const part of [new Date(run.at_ms).toLocaleString(), run.plugin, run.frame, `${run.ms} мс`]) meta.append(text("span", part));
            item.append(meta, text("div", RESULTS[run.result] ?? run.result, run.result === "failed" ? "why" : "heard"));
            if (run.message) item.append(text("div", run.message, "answer"));
            return item;
          })
        : [text("p", "Запусков пока не было.", "empty")]),
    );
  } catch (error) {
    $("job-list").replaceChildren(text("p", `Не удалось загрузить: ${error.message}`, "empty"));
  }
}

// ---- assistant history -----------------------------------------------------

const TRIGGERS = { key: "кнопка", voice: "голос", console: "консоль", "": "текст" };
const REASONS = {
  "stt: not addressed": "не обращались к рамке",
  "stt: no speech": "не речь",
  "stt: wake only": "только имя, ждём вопрос",
};

async function loadHistory() {
  try {
    const answer = await fetch("/api/history?limit=100");
    if (!answer.ok) throw new Error(await answer.text());
    const { turns } = await answer.json();
    renderTurns(turns);
  } catch (error) {
    $("turns").replaceChildren(text("p", `Не удалось загрузить: ${error.message}`, "empty"));
  }
}

function text(tag, content, className) {
  const element = document.createElement(tag);
  element.textContent = content;
  if (className) element.className = className;
  return element;
}

function renderTurns(turns) {
  const showRejected = $("history-rejected").checked;
  const shown = turns.filter((turn) => showRejected || !turn.rejected);
  $("history-count").textContent = `${shown.length} из ${turns.length}`;
  if (!shown.length) {
    $("turns").replaceChildren(text("p", "Пока ничего: спросите рамку.", "empty"));
    return;
  }
  $("turns").replaceChildren(
    ...shown.map((turn) => {
      const item = document.createElement("article");
      item.className = turn.rejected ? "turn rejected" : "turn";
      const when = new Date(turn.at_ms).toLocaleString();
      const timings = [
        turn.stt_ms != null && `распознавание ${turn.stt_ms} мс`,
        turn.llm_ms != null && `ответ ${turn.llm_ms} мс`,
        turn.tts_ms != null && `голос ${turn.tts_ms} мс`,
      ].filter(Boolean);
      const meta = document.createElement("div");
      meta.className = "meta";
      for (const part of [when, turn.peer, TRIGGERS[turn.trigger] ?? turn.trigger, ...timings]) {
        if (part) meta.append(text("span", part));
      }
      item.append(meta);
      if (turn.heard != null) item.append(text("div", turn.heard || "(пусто)", "heard"));
      if (turn.rejected) item.append(text("div", REASONS[turn.rejected] ?? turn.rejected, "why"));
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
  say("ask-answer", "Думаю…", "");
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
$("history-rejected").addEventListener("change", loadHistory);
setInterval(() => {
  if (!$("tab-assistant").hidden && document.visibilityState === "visible") loadHistory();
}, 5000);

// ---- settings --------------------------------------------------------------

// How each key is edited, in the order the form shows them.
const FIELDS = [
  { key: "wake", title: "Имя рамки (основы слов через запятую)", kind: "list",
    note: "Голосовой вопрос без этого слова остаётся без ответа; вопрос по кнопке — нет." },
  { key: "llm_system", title: "Системная подсказка модели", kind: "textarea",
    note: "Пусто — подсказка по умолчанию из llm-local.py." },
  { key: "stt_prompt", title: "Подсказка распознаванию", kind: "textarea",
    note: "Фраза, которую распознавание считает только что услышанной: имя рамки и редкие названия. Например: «Рамка, покажи погоду в Эспоо, Хельсинки и Тампере». Действует со следующего вопроса." },
  { key: "llm_url", title: "Адрес модели (chat completions)", kind: "text" },
  { key: "token", title: "Ключ рамок (Bearer)", kind: "secret" },
  { key: "ui_password", title: "Пароль этой страницы", kind: "secret" },
  { key: "ui_hosts", title: "Дополнительные имена этого моста", kind: "list",
    note: "Имена, под которыми страница открывается кроме IP и локальных имён." },
  { key: "addr", title: "Адрес и порт", kind: "text" },
  { key: "stt", title: "Распознавание", kind: "text" },
  { key: "llm", title: "Модель", kind: "text" },
  { key: "tts", title: "Синтез речи", kind: "text" },
];

let loaded = null;

async function loadSettings() {
  try {
    const answer = await fetch("/api/settings");
    if (!answer.ok) throw new Error(await answer.text());
    loaded = await answer.json();
    renderSettings(loaded);
    $("settings-file").textContent = `Файл: ${loaded.file}`;
    say("settings-message", "", "");
  } catch (error) {
    say("settings-message", `Не удалось загрузить: ${error.message}`, "bad");
  }
}

function renderSettings({ values, pinned, restart }) {
  const form = $("settings-form");
  form.replaceChildren(
    ...FIELDS.map((field) => {
      const box = document.createElement("div");
      box.className = "field";
      const label = text("label", field.title);
      label.htmlFor = `setting-${field.key}`;
      if (restart.includes(field.key)) label.append(text("span", "после перезапуска", "badge"));
      if (pinned.includes(field.key)) label.append(text("span", "задано в командной строке", "badge warn"));
      let input;
      if (field.kind === "textarea") {
        input = document.createElement("textarea");
        input.value = values[field.key] ?? "";
      } else {
        input = document.createElement("input");
        input.type = field.kind === "secret" ? "password" : "text";
        if (field.kind === "secret") {
          // The value never comes back from the bridge: empty keeps it, "-" clears.
          input.placeholder = values[field.key] ? "задан; пусто — не менять, «-» — убрать" : "не задан";
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
      if (field.note) box.append(text("p", field.note, "note"));
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
    } else if (input.value !== (before ?? "")) {
      change[field.key] = input.value;
    }
  }
  return change;
}

$("settings-save").addEventListener("click", async () => {
  const change = changes();
  if (!Object.keys(change).length) {
    say("settings-message", "Нечего сохранять.", "");
    return;
  }
  try {
    const answer = await post("/api/settings", change);
    const body = await answer.text();
    if (!answer.ok) throw new Error(body.trim());
    const { changed, restart } = JSON.parse(body);
    const after = restart.length ? ` Перезапустите мост, чтобы применить: ${restart.join(", ")}.` : "";
    say("settings-message", `Сохранено: ${changed.join(", ") || "ничего"}.${after}`, "ok");
    await loadSettings();
    if (restart.length) say("settings-message", `Сохранено.${after}`, "ok");
    loadStatus();
  } catch (error) {
    say("settings-message", error.message, "bad");
  }
});
$("settings-reload").addEventListener("click", loadSettings);

// ---- start -----------------------------------------------------------------

loadStatus();
setInterval(loadStatus, 30000);
showTab();
// Enter in a field must not submit the form and reload the page.
$("settings-form").addEventListener("submit", (event) => event.preventDefault());
