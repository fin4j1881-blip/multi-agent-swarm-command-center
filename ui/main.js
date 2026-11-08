/* Swarm Command Center – plain JS frontend (no bundler).
   Talks to the Rust backend through Tauri v2 global IPC. */

const tauri = window.__TAURI__;
const invoke = tauri?.core?.invoke;
const listen = tauri?.event?.listen;

const $ = (id) => document.getElementById(id);

const state = {
  running: false,
  logs: [],
  statsTimer: null,
  saveTimer: null,
};

const IDE_HINTS = {
  opencode:
    "`opencode` PATH'te veya bilinen kurulum yerlerinde olmalı (gerekirse OPENCODE_BIN). Endpoint doldurursan CLI yerine HTTP köprüsü kullanılır.",
  claude_code:
    "`claude` CLI kurulu olmalı; prompt stdin ile gönderilir (`claude -p --model ...`).",
  cursor_agent:
    "`cursor-agent` CLI kurulu olmalı; prompt argüman olarak gönderilir (`cursor-agent -p ...`).",
  custom_http:
    "POST {model, system, prompt} → {content|text|output} sözleşmesini konuşan yerel köprü. Endpoint zorunlu.",
  custom_cli:
    "Komut ve argümanlar. Argümanlarda `{prompt}` yer tutucusu desteklenir; yoksa prompt sona eklenir.",
};

/* ---------------- toasts ---------------- */

function toast(message, level = "ok") {
  const host = $("toasts");
  if (!host) return;
  const el = document.createElement("div");
  el.className = `toast ${level}`;
  el.textContent = String(message);
  host.appendChild(el);
  setTimeout(() => {
    el.style.opacity = "0";
    el.style.transition = "opacity .2s";
    setTimeout(() => el.remove(), 220);
  }, level === "error" ? 9000 : 5000);
}

/* ---------------- provider form ---------------- */

function sourceFor(prefix) {
  const active = document.querySelector(`[data-seg="${prefix}"] .seg.active`);
  return active ? active.dataset.source : "api";
}

function setSource(prefix, source) {
  document.querySelectorAll(`[data-seg="${prefix}"] .seg`).forEach((button) => {
    const active = button.dataset.source === source;
    button.classList.toggle("active", active);
    button.setAttribute("aria-pressed", String(active));
  });
  applyVisibility(prefix);

  if (source === "api") {
    applyApiDefaults(prefix, false);
    if ($(`${prefix}ApiKey`).value.trim()) loadModels(prefix);
  } else {
    loadModels(prefix);
  }
  persist();
}

function applyVisibility(prefix) {
  const source = sourceFor(prefix);
  const apiFields = document.querySelector(`[data-fields="${prefix}-api"]`);
  const ideFields = document.querySelector(`[data-fields="${prefix}-ide"]`);
  if (apiFields) apiFields.classList.toggle("hidden", source !== "api");
  if (ideFields) ideFields.classList.toggle("hidden", source !== "ide");

  const ideKind = $(`${prefix}IdeKind`).value;
  const endpointField = document.querySelector(`[data-ide-endpoint="${prefix}"]`);
  const commandFields = document.querySelectorAll(`[data-ide-command="${prefix}"]`);
  const isCli = ideKind === "custom_cli";

  if (endpointField) endpointField.classList.toggle("hidden", isCli);
  commandFields.forEach((el) => el.classList.toggle("hidden", !isCli));

  const hint = $(`${prefix}IdeHint`);
  if (hint) {
    hint.textContent = IDE_HINTS[ideKind] || "";
    hint.classList.toggle("hidden", source !== "ide");
  }

  const endpoint = $(`${prefix}IdeEndpoint`);
  if (endpoint) {
    endpoint.placeholder =
      ideKind === "custom_http" ? "http://127.0.0.1:4096" : "opsiyonel — doldurulursa HTTP köprüsü";
  }
}

function parseArgs(raw) {
  const out = [];
  let current = "";
  let quote = null;
  for (const ch of raw.trim()) {
    if (quote) {
      if (ch === quote) quote = null;
      else current += ch;
    } else if (ch === '"' || ch === "'") {
      quote = ch;
    } else if (/\s/.test(ch)) {
      if (current) {
        out.push(current);
        current = "";
      }
    } else {
      current += ch;
    }
  }
  if (current) out.push(current);
  return out;
}

const API_BASE_DEFAULTS = {
  openrouter: "https://openrouter.ai/api/v1",
  anthropic: "https://api.anthropic.com",
  openai: "https://api.openai.com/v1",
  deepseek: "https://api.deepseek.com/v1",
  custom: "",
};

function applyApiDefaults(prefix, force) {
  const kind = $(`${prefix}ApiKind`).value;
  const input = $(`${prefix}BaseUrl`);
  const fallback = API_BASE_DEFAULTS[kind] || "";
  const current = input.value.trim();

  if (kind === "custom") {
    const known = Object.values(API_BASE_DEFAULTS).filter(Boolean);
    if (force && known.includes(current)) input.value = "";
    input.dataset.auto = "0";
  } else if (force || !current || input.dataset.auto === "1") {
    input.value = fallback;
    input.dataset.auto = "1";
  }
  input.placeholder = fallback || "http://127.0.0.1:11434/v1";
}

function effectiveModel(prefix, source) {
  const select = $(source === "api" ? `${prefix}Model` : `${prefix}IdeModel`);
  const custom = $(source === "api" ? `${prefix}ModelCustom` : `${prefix}IdeModelCustom`);
  return select.value === "__custom__" ? custom.value.trim() : select.value.trim();
}

function modelElements(prefix, source) {
  const api = source === "api";
  return {
    select: $(api ? `${prefix}Model` : `${prefix}IdeModel`),
    fallbackSelect: $(api ? `${prefix}FallbackModel` : `${prefix}IdeFallbackModel`),
    custom: $(api ? `${prefix}ModelCustom` : `${prefix}IdeModelCustom`),
    note: $(api ? `${prefix}ModelNote` : `${prefix}IdeModelNote`),
    optional: !api,
  };
}

function syncCustomVisibility(prefix, source) {
  const { select, custom } = modelElements(prefix, source);
  const isCustom = select.value === "__custom__";
  custom.classList.toggle("hidden", !isCustom);
  if (isCustom) custom.focus();
}

function resetModelChoice(prefix, source) {
  const { select, fallbackSelect, custom, note } = modelElements(prefix, source);
  select.value = "";
  if (fallbackSelect) fallbackSelect.value = "";
  custom.value = "";
  custom.classList.add("hidden");
  note.textContent = "";
  note.classList.remove("error");
}

function fillModelSelect(prefix, source, catalog, error) {
  const { select, fallbackSelect, custom, note, optional } = modelElements(prefix, source);
  const previous = select.value;
  const previousFallback = fallbackSelect ? fallbackSelect.value : "";
  select.innerHTML = "";
  if (fallbackSelect) fallbackSelect.innerHTML = "";

  const placeholder = document.createElement("option");
  placeholder.value = "";
  placeholder.textContent = error
    ? "— liste alınamadı —"
    : optional
      ? "CLI varsayılanı (boş)"
      : "— model seç —";
  select.appendChild(placeholder);

  if (fallbackSelect) {
    const fbPlaceholder = document.createElement("option");
    fbPlaceholder.value = "";
    fbPlaceholder.textContent = "— yok (tek model kullan) —";
    fallbackSelect.appendChild(fbPlaceholder);
  }

  if (catalog) {
    for (const model of catalog.models) {
      const option = document.createElement("option");
      option.value = model.id;
      option.textContent =
        model.name && model.name !== model.id ? `${model.name} · ${model.id}` : model.id;
      if (model.context) option.title = `bağlam: ${model.context} token`;
      select.appendChild(option);

      if (fallbackSelect) {
        const fbOption = document.createElement("option");
        fbOption.value = model.id;
        fbOption.textContent = option.textContent;
        if (model.context) fbOption.title = option.title;
        fallbackSelect.appendChild(fbOption);
      }
    }
  }

  const customOption = document.createElement("option");
  customOption.value = "__custom__";
  customOption.textContent = "Özel model adı…";
  select.appendChild(customOption);

  if (previous && previous !== "__custom__") {
    const exists = Array.from(select.options).some((option) => option.value === previous);
    if (!exists) {
      const option = document.createElement("option");
      option.value = previous;
      option.textContent = previous;
      select.insertBefore(option, customOption);
    }
    select.value = previous;
  } else if (previous === "__custom__") {
    select.value = "__custom__";
    custom.classList.remove("hidden");
  }

  if (fallbackSelect && previousFallback) {
    const exists = Array.from(fallbackSelect.options).some((option) => option.value === previousFallback);
    if (!exists) {
      const option = document.createElement("option");
      option.value = previousFallback;
      option.textContent = previousFallback;
      fallbackSelect.appendChild(option);
    }
    fallbackSelect.value = previousFallback;
  }

  select.disabled = false;
  if (fallbackSelect) fallbackSelect.disabled = false;
  note.classList.toggle("error", Boolean(error));
  note.textContent = error ? String(error) : catalog?.note || "";
}

const modelRequest = { arch: 0, coder: 0 };

async function loadModels(prefix) {
  if (!invoke) return;

  const source = sourceFor(prefix);
  const { select, custom, note } = modelElements(prefix, source);
  const wasCustom = select.value === "__custom__";
  const customText = custom.value;
  const request = ++modelRequest[prefix];

  select.disabled = true;
  note.classList.remove("error");
  note.textContent = "Modeller getiriliyor…";
  document.querySelectorAll(`[data-models="${prefix}"]`).forEach((button) => {
    button.classList.add("spinning");
  });

  try {
    const catalog = await invoke("list_models", {
      config: providerConfig(prefix),
      projectPath: $("projectPath").value.trim() || null,
    });
    if (request !== modelRequest[prefix]) return;
    fillModelSelect(prefix, source, catalog, null);
  } catch (error) {
    if (request !== modelRequest[prefix]) return;
    const message = error instanceof Error ? error.message : String(error);
    fillModelSelect(prefix, source, null, message);
  } finally {
    if (request === modelRequest[prefix]) {
      if (wasCustom) {
        const els = modelElements(prefix, source);
        els.select.value = "__custom__";
        els.custom.value = customText;
        els.custom.classList.remove("hidden");
      }
      document.querySelectorAll(`[data-models="${prefix}"]`).forEach((button) => {
        button.classList.remove("spinning");
      });
    }
  }
}

function providerConfig(prefix) {
  const source = sourceFor(prefix);
  const model = effectiveModel(prefix, source);
  const apiKey = $(`${prefix}ApiKey`).value.trim();

  const fallbackSelect = $(source === "api" ? `${prefix}FallbackModel` : `${prefix}IdeFallbackModel`);
  const fallbackModel = fallbackSelect ? fallbackSelect.value.trim() : "";
  const fallbackModels = fallbackModel && fallbackModel !== model ? [fallbackModel] : [];

  return {
    source,
    apiKind: $(`${prefix}ApiKind`).value,
    ideKind: $(`${prefix}IdeKind`).value,
    model,
    fallbackModels,
    apiKey: apiKey || null,
    baseUrl: $(`${prefix}BaseUrl`).value.trim() || null,
    ideEndpoint: $(`${prefix}IdeEndpoint`).value.trim() || null,
    ideCommand: $(`${prefix}IdeCommand`).value.trim() || null,
    ideArgs: parseArgs($(`${prefix}IdeArgs`).value),
    temperature: 0.2,
    maxTokens: Number($("maxTokens").value) || 8192,
    timeoutSecs: $(`${prefix}IdeKind`).value === "claude_code" ? 900 : 600,
  };
}

function buildConfig() {
  return {
    projectPath: $("projectPath").value.trim(),
    vision: $("vision").value.trim(),
    instructions: $("instructions").value.trim(),
    architect: providerConfig("arch"),
    coder: providerConfig("coder"),
    maxAttempts: Number($("maxAttempts").value) || 3,
    maxRounds: Number($("maxRounds").value) || 3,
    maxTasks: Number($("maxTasks").value) || 6,
    contextChars: Number($("contextChars").value) || 24000,
    treeEntries: 500,
    autoApply: $("autoApply").checked,
    dryRun: $("dryRun").checked,
    verifyBuild: $("verifyBuild").checked,
  };
}

/* ---------------- console ---------------- */

function consoleEl() {
  return $("console");
}

function removeEmptyState() {
  const empty = consoleEl().querySelector(".empty");
  if (empty) empty.remove();
}

function renderEmptyState() {
  const host = consoleEl();
  host.innerHTML = "";
  const wrap = document.createElement("div");
  wrap.className = "empty";
  const icon = document.createElement("div");
  icon.className = "empty-icon";
  icon.textContent = "⌘";
  const line1 = document.createElement("p");
  line1.textContent = "Komutanlık henüz başlatılmadı.";
  const line2 = document.createElement("p");
  line2.className = "muted";
  line2.textContent =
    "Proje klasörünü ve vizyonu gir, komutan ve işçi kaynaklarını seç, ardından başlat.";
  wrap.append(icon, line1, line2);
  host.appendChild(wrap);
}

function buildLogRow(event) {
  const row = document.createElement("div");
  row.className = `log ${event.level || "info"}`;

  const time = document.createElement("span");
  time.className = "time";
  time.textContent = event.timestamp || "";

  const phase = document.createElement("span");
  phase.className = "phase";
  phase.textContent = event.phase || "";

  const msg = document.createElement("div");
  msg.className = "msg";

  const text = document.createElement("span");
  text.textContent = event.message || "";
  msg.appendChild(text);

  if (event.detail) {
    const details = document.createElement("details");
    const summary = document.createElement("summary");
    summary.textContent = "detay";
    const pre = document.createElement("pre");
    pre.textContent = event.detail;
    details.append(summary, pre);
    msg.appendChild(details);
  }

  row.append(time, phase, msg);
  return row;
}

/* Appends a batch with a single DOM insertion and a single scroll, so replaying
   the backend's log history does not force one layout per line. */
function addLogs(events) {
  if (!events?.length) return;
  removeEmptyState();

  const host = consoleEl();
  const fragment = document.createDocumentFragment();
  for (const event of events) {
    fragment.appendChild(buildLogRow(event));
    state.logs.push(event);
  }
  host.appendChild(fragment);

  while (host.children.length > 600) host.firstElementChild?.remove();
  if (state.logs.length > 1200) state.logs.splice(0, state.logs.length - 1200);

  if ($("autoscroll").checked) host.scrollTop = host.scrollHeight;
}

function clearConsole() {
  state.logs = [];
  renderEmptyState();
}

/* ---------------- status & stats ---------------- */

function setRunning(running) {
  state.running = running;

  const pill = $("statusPill");
  pill.className = `pill ${running ? "running" : "idle"}`;
  $("statusText").textContent = running ? "Çalışıyor" : "Hazır";
  $("startBtn").disabled = running;
  $("stopBtn").disabled = !running;
  $("footStatus").textContent = running ? "Ajan döngüsü aktif" : "Bekliyor";
}

function updateFootHint() {
  const attempts = Number($("maxAttempts").value) || 3;
  $("footHint").textContent = `Yalnızca onaylanan dosyalar yazılır · görev başına maks. ${attempts} deneme`;
}

function applyStats(stats) {
  if (!stats) return;
  $("statRound").textContent = stats.rounds ?? 0;
  $("statApplied").textContent = stats.tasksApplied ?? 0;
  $("statRejected").textContent = stats.tasksRejected ?? 0;
  $("statSkipped").textContent = stats.tasksSkipped ?? 0;
  $("statRevisions").textContent = stats.revisions ?? 0;
  $("statFiles").textContent = stats.filesWritten ?? 0;
}

function scheduleStats() {
  if (state.statsTimer) return;
  state.statsTimer = setTimeout(async () => {
    state.statsTimer = null;
    try {
      const currentPath = $("projectPath").value.trim();
      const status = await invoke("get_stats", {
        projectPath: currentPath || null,
      });
      applyStats(status.stats);
      if (status.running !== state.running) setRunning(status.running);
    } catch {
      /* ignore transient IPC errors */
    }
  }, 350);
}

/* ---------------- persistence (no secrets) ---------------- */

const PERSISTED_FIELDS = [
  "projectPath",
  "vision",
  "instructions",
  "archApiKind",
  "archBaseUrl",
  "archIdeKind",
  "archIdeEndpoint",
  "archIdeCommand",
  "archIdeArgs",
  "coderApiKind",
  "coderBaseUrl",
  "coderIdeKind",
  "coderIdeEndpoint",
  "coderIdeCommand",
  "coderIdeArgs",
  "maxAttempts",
  "maxRounds",
  "maxTasks",
  "contextChars",
  "maxTokens",
];

function projectStorageKey(path) {
  const clean = (path || "").trim().replace(/\/+$/, "");
  return clean ? `swarm.project.${clean}` : null;
}

function persist() {
  clearTimeout(state.saveTimer);
  state.saveTimer = setTimeout(() => {
    try {
      const currentPath = $("projectPath").value.trim();
      const data = { source: { arch: sourceFor("arch"), coder: sourceFor("coder") } };
      for (const id of PERSISTED_FIELDS) {
        const el = $(id);
        if (el) data[id] = el.value;
      }
      data.autoApply = $("autoApply").checked;
      data.dryRun = $("dryRun").checked;
      data.verifyBuild = $("verifyBuild").checked;
      data.autoscroll = $("autoscroll").checked;
      data.models = {};
      for (const prefix of ["arch", "coder"]) {
        data.models[prefix] = {
          api: effectiveModel(prefix, "api"),
          apiFallback: $(`${prefix}FallbackModel`).value,
          ide: effectiveModel(prefix, "ide"),
          ideFallback: $(`${prefix}IdeFallbackModel`).value,
        };
      }
      // Save global last-state
      localStorage.setItem("swarm.settings.v1", JSON.stringify(data));

      // Also save project-specific vision & instructions under project key
      const pKey = projectStorageKey(currentPath);
      if (pKey) {
        const projectData = {
          vision: $("vision").value,
          instructions: $("instructions").value,
          models: data.models,
        };
        localStorage.setItem(pKey, JSON.stringify(projectData));
      }
    } catch {
      /* storage unavailable */
    }
  }, 300);
}

const projectRunningMap = {};

function getKnownProjects() {
  const keys = [];
  try {
    for (let i = 0; i < localStorage.length; i++) {
      const k = localStorage.key(i);
      if (k && k.startsWith("swarm.project.")) {
        keys.push(k.substring("swarm.project.".length));
      }
    }
  } catch {}
  const current = $("projectPath")?.value?.trim();
  if (current && !keys.includes(current)) keys.push(current);
  return keys.sort();
}

function renderProjectSidebar() {
  const listEl = $("projectList");
  if (!listEl) return;
  listEl.innerHTML = "";

  const currentPath = $("projectPath")?.value?.trim();
  const projects = getKnownProjects();

  for (const path of projects) {
    const item = document.createElement("div");
    const isRunning = Boolean(projectRunningMap[path]);
    const isActive = path === currentPath;

    item.className = `project-item ${isActive ? "active" : ""} ${isRunning ? "running" : ""}`;

    const parts = path.split("/").filter(Boolean);
    const shortName = parts[parts.length - 1] || path;

    item.innerHTML = `
      <div class="p-name">
        <span class="p-dot" title="${isRunning ? "Çalışıyor" : "Boşta"}"></span>
        <span>${shortName}</span>
        <button class="p-delete-btn" type="button" title="Projeyi Arayüzden Kaldır">×</button>
      </div>
      <div class="p-path" title="${path}">${path}</div>
    `;

    item.querySelector(".p-delete-btn").addEventListener("click", (e) => {
      e.stopPropagation();
      removeProjectFromUi(path);
    });

    item.addEventListener("click", () => {
      if (path !== $("projectPath").value.trim()) {
        switchProject(path);
      }
    });

    listEl.appendChild(item);
  }
}

function removeProjectFromUi(path) {
  const isRunning = Boolean(projectRunningMap[path]);
  if (isRunning) {
    toast("Çalışan proje kaldırılamaz. Önce durdurun.", "error");
    return;
  }

  const pKey = projectStorageKey(path);
  if (pKey) {
    try {
      localStorage.removeItem(pKey);
    } catch {}
  }

  const currentPath = $("projectPath").value.trim();
  if (currentPath === path) {
    const remaining = getKnownProjects().filter((p) => p !== path);
    if (remaining.length > 0) {
      switchProject(remaining[0]);
    } else {
      $("projectPath").value = "";
      $("vision").value = "";
      $("visionCount").textContent = "0";
      $("instructions").value = "";
      clearConsole();
      applyStats({});
      setRunning(false);
      renderProjectSidebar();
      persist();
    }
  } else {
    renderProjectSidebar();
  }

  toast("Proje arayüzden kaldırıldı (dosyalarınıza dokunulmadı).", "ok");
}

async function triggerAiProjectAnalysis() {
  const currentPath = $("projectPath").value.trim();
  if (!currentPath) {
    toast("Önce bir proje klasörü seçmelisiniz.", "error");
    return;
  }

  const btn = $("aiAnalyzeBtn");
  const origText = btn.textContent;
  btn.disabled = true;
  btn.textContent = "✨ Taranıyor…";

  try {
    const res = await invoke("analyze_project", {
      projectPath: currentPath,
      config: providerConfig("arch"),
    });

    if (res.vision) {
      $("vision").value = res.vision;
      $("visionCount").textContent = `${res.vision.trim().length}`;
    }
    if (res.instructions) {
      $("instructions").value = res.instructions;
    }
    persist();
    toast("Proje başarıyla analiz edildi! Vizyon ve talimatlar dolduruldu.", "ok");
  } catch (err) {
    toast(`AI analizi başarısız: ${err}`, "error");
  } finally {
    btn.disabled = false;
    btn.textContent = origText;
  }
}

async function switchProject(newPath) {
  persist(); // Save current project
  $("projectPath").value = newPath;
  loadProjectData(newPath);
  renderProjectSidebar();

  // Load status and logs for the switched project
  try {
    const status = await invoke("get_status", { projectPath: newPath });
    applyStats(status.stats);
    setRunning(Boolean(status.running));
    clearConsole();
    if (status.logs?.length) {
      addLogs(status.logs);
    }
  } catch (e) {
    /* ignore */
  }
}

function loadProjectData(path) {
  const pKey = projectStorageKey(path);
  if (!pKey) return;
  try {
    const raw = localStorage.getItem(pKey);
    if (!raw) return;
    const pData = JSON.parse(raw);
    if (typeof pData.vision === "string") {
      $("vision").value = pData.vision;
      $("visionCount").textContent = `${pData.vision.trim().length}`;
    }
    if (typeof pData.instructions === "string") {
      $("instructions").value = pData.instructions;
    }
    if (pData.models) {
      for (const prefix of ["arch", "coder"]) {
        const saved = pData.models[prefix];
        if (!saved) continue;
        setSelectValue($(`${prefix}Model`), saved.api);
        setSelectValue($(`${prefix}FallbackModel`), saved.apiFallback);
        setSelectValue($(`${prefix}IdeModel`), saved.ide);
        setSelectValue($(`${prefix}IdeFallbackModel`), saved.ideFallback);
      }
    }
  } catch {
    /* ignore */
  }
}

function setSelectValue(select, value) {
  if (!select || !value) return;
  if (!Array.from(select.options).some((option) => option.value === value)) {
    const option = document.createElement("option");
    option.value = value;
    option.textContent = value;
    const custom = Array.from(select.options).find((o) => o.value === "__custom__");
    select.insertBefore(option, custom || null);
  }
  select.value = value;
}

function restore() {
  let data;
  try {
    data = JSON.parse(localStorage.getItem("swarm.settings.v1") || "null");
  } catch {
    data = null;
  }
  if (!data) return;

  for (const id of PERSISTED_FIELDS) {
    if (data[id] !== undefined && $(id)) $(id).value = data[id];
  }
  if (typeof data.autoApply === "boolean") $("autoApply").checked = data.autoApply;
  if (typeof data.dryRun === "boolean") $("dryRun").checked = data.dryRun;
  if (typeof data.verifyBuild === "boolean") $("verifyBuild").checked = data.verifyBuild;
  if (typeof data.autoscroll === "boolean") $("autoscroll").checked = data.autoscroll;

  /* Model lists are fetched on demand, so put the remembered choice into the
     (still empty) selects; fillModelSelect keeps it when the list arrives. */
  for (const prefix of ["arch", "coder"]) {
    const saved = data.models?.[prefix];
    if (!saved) continue;
    setSelectValue($(`${prefix}Model`), saved.api);
    setSelectValue($(`${prefix}FallbackModel`), saved.apiFallback);
    setSelectValue($(`${prefix}IdeModel`), saved.ide);
    setSelectValue($(`${prefix}IdeFallbackModel`), saved.ideFallback);
  }

  if (data.source?.arch) {
    document.querySelectorAll('[data-seg="arch"] .seg').forEach((button) => {
      button.classList.toggle("active", button.dataset.source === data.source.arch);
    });
  }
  if (data.source?.coder) {
    document.querySelectorAll('[data-seg="coder"] .seg').forEach((button) => {
      button.classList.toggle("active", button.dataset.source === data.source.coder);
    });
  }
}

/* ---------------- actions ---------------- */

async function pickFolder() {
  try {
    const folder = await invoke("select_folder");
    if (folder) {
      $("projectPath").value = folder;
      const pKey = projectStorageKey(folder);
      const hasExisting = pKey && localStorage.getItem(pKey);

      if (hasExisting) {
        loadProjectData(folder);
        persist();
        renderProjectSidebar();
      } else {
        // Yeni bir proje eklendi! Otomatik AI analizi teklif et ve çalıştır
        renderProjectSidebar();
        persist();
        triggerAiProjectAnalysis();
      }
    }
  } catch (error) {
    toast(`Klasör seçilemedi: ${error}`, "error");
  }
}

async function testProvider(prefix) {
  const button = document.querySelector(`[data-test="${prefix}"]`);
  const original = button.textContent;
  button.disabled = true;
  button.textContent = "…";
  try {
    const message = await invoke("check_provider", {
      config: providerConfig(prefix),
      projectPath: $("projectPath").value.trim() || null,
    });
    toast(message, "ok");
  } catch (error) {
    toast(String(error), "error");
  } finally {
    button.disabled = false;
    button.textContent = original;
  }
}

async function start() {
  const config = buildConfig();

  if (!config.projectPath) {
    toast("Önce proje klasörünü seç.", "error");
    return;
  }
  if (config.vision.length < 20) {
    toast("Proje vizyonu en az 20 karakter olmalı.", "error");
    return;
  }

  for (const [role, provider] of [
    ["Komutan", config.architect],
    ["İşçi", config.coder],
  ]) {
    if (provider.source === "api" && !provider.model) {
      toast(`${role} için listeden bir model seç (⟳ ile listeyi yenile).`, "error");
      return;
    }
  }

  $("startBtn").disabled = true;
  clearConsole();
  applyStats({});

  try {
    const root = await invoke("start_command_center", { config });
    projectRunningMap[config.projectPath] = true;
    renderProjectSidebar();
    setRunning(true);
    toast(`Komutanlık başladı: ${root}`, "ok");
  } catch (error) {
    $("startBtn").disabled = false;
    toast(String(error), "error");
    try {
      const status = await invoke("get_status", { projectPath: config.projectPath });
      clearConsole();
      addLogs(status.logs);
    } catch {
      /* keep the empty console */
    }
  }
}

async function stop() {
  const currentPath = $("projectPath").value.trim();
  $("stopBtn").disabled = true;
  $("statusPill").className = "pill stopping";
  $("statusText").textContent = "Durduruluyor…";
  try {
    await invoke("stop_command_center", { projectPath: currentPath || null });
  } catch (error) {
    toast(String(error), "error");
    setRunning(state.running);
  }
}

/* ---------------- wiring ---------------- */

function bind() {
  $("pickFolderBtn").addEventListener("click", pickFolder);
  $("startBtn").addEventListener("click", start);
  $("stopBtn").addEventListener("click", stop);

  const aiAnalyzeBtn = $("aiAnalyzeBtn");
  if (aiAnalyzeBtn) aiAnalyzeBtn.addEventListener("click", triggerAiProjectAnalysis);

  const newWindowHandler = async () => {
    try {
      await invoke("open_new_window");
    } catch (e) {
      toast(`Yeni pencere açılamadı: ${e}`, "error");
    }
  };

  const newWindowBtn = $("newWindowBtn");
  if (newWindowBtn) newWindowBtn.addEventListener("click", newWindowHandler);

  window.addEventListener("keydown", (e) => {
    if ((e.ctrlKey || e.metaKey) && (e.key === "n" || e.key === "N")) {
      e.preventDefault();
      newWindowHandler();
    }
  });

  const addProjectBtn = $("addProjectSidebarBtn");
  if (addProjectBtn) addProjectBtn.addEventListener("click", pickFolder);

  $("projectPath").addEventListener("change", () => {
    loadProjectData($("projectPath").value);
    persist();
    renderProjectSidebar();
  });

  document.querySelectorAll(".seg").forEach((button) => {
    button.addEventListener("click", () => {
      const prefix = button.closest("[data-seg]").dataset.seg;
      setSource(prefix, button.dataset.source);
    });
  });

  document.querySelectorAll("[data-test]").forEach((button) => {
    button.addEventListener("click", () => testProvider(button.dataset.test));
  });

  document.querySelectorAll("[data-reveal]").forEach((button) => {
    button.addEventListener("click", () => {
      const input = $(button.dataset.reveal);
      const show = input.type === "password";
      input.type = show ? "text" : "password";
      button.textContent = show ? "gizle" : "göster";
    });
  });

  for (const prefix of ["arch", "coder"]) {
    $(`${prefix}ApiKind`).addEventListener("change", () => {
      applyApiDefaults(prefix, true);
      resetModelChoice(prefix, "api");
      if (sourceFor(prefix) === "api") loadModels(prefix);
      persist();
    });

    $(`${prefix}IdeKind`).addEventListener("change", () => {
      applyVisibility(prefix);
      resetModelChoice(prefix, "ide");
      if (sourceFor(prefix) === "ide") loadModels(prefix);
      persist();
    });

    $(`${prefix}BaseUrl`).addEventListener("input", (event) => {
      event.target.dataset.auto = "0";
    });

    for (const [source, selectId] of [
      ["api", `${prefix}Model`],
      ["ide", `${prefix}IdeModel`],
    ]) {
      const select = $(selectId);
      const custom = $(selectId + "Custom");
      select.addEventListener("change", () => {
        syncCustomVisibility(prefix, source);
        persist();
      });
      custom.addEventListener("input", persist);
    }

    $(`${prefix}FallbackModel`).addEventListener("change", persist);
    $(`${prefix}IdeFallbackModel`).addEventListener("change", persist);

    const keyInput = $(`${prefix}ApiKey`);
    let keyTimer = null;
    keyInput.addEventListener("input", () => {
      clearTimeout(keyTimer);
      keyTimer = setTimeout(() => {
        if (sourceFor(prefix) === "api" && keyInput.value.trim()) loadModels(prefix);
      }, 900);
    });
    keyInput.addEventListener("change", () => {
      if (sourceFor(prefix) === "api" && keyInput.value.trim()) loadModels(prefix);
    });

    document.querySelectorAll(`[data-models="${prefix}"]`).forEach((button) => {
      button.addEventListener("click", () => loadModels(prefix));
    });

    document.querySelector(`[data-reset-base="${prefix}"]`).addEventListener("click", () => {
      applyApiDefaults(prefix, true);
      persist();
    });
  }

  $("vision").addEventListener("input", () => {
    $("visionCount").textContent = `${$("vision").value.trim().length}`;
    persist();
  });

  document.querySelectorAll("input, select, textarea").forEach((el) => {
    el.addEventListener("change", persist);
    if (el.type !== "checkbox") el.addEventListener("input", persist);
  });

  $("clearBtn").addEventListener("click", clearConsole);

  $("copyBtn").addEventListener("click", async () => {
    if (!state.logs.length) {
      toast("Kopyalanacak log yok.", "warn");
      return;
    }
    const text = state.logs
      .map((ev) => `[${ev.timestamp || "--:--:--"}] [${ev.phase || "log"}] ${ev.message || ""}${ev.detail ? `\n${ev.detail}` : ""}`)
      .join("\n");
    try {
      await navigator.clipboard.writeText(text);
      toast("Loglar panoya kopyalandı.", "ok");
    } catch {
      toast("Panoya kopyalama başarısız.", "error");
    }
  });

  window.addEventListener("beforeunload", persist);
}

async function bootstrap() {
  if (!invoke || !listen) {
    toast(
      "Tauri IPC bulunamadı. Bu arayüzü `tauri dev` ile çalıştır (tarayıcıda değil).",
      "error"
    );
    return;
  }

  restore();
  bind();

  for (const prefix of ["arch", "coder"]) {
    applyVisibility(prefix);
    applyApiDefaults(prefix, false);
  }
  $("visionCount").textContent = `${$("vision").value.trim().length}`;
  updateFootHint();
  $("maxAttempts").addEventListener("input", updateFootHint);

  await listen("agent-log", (event) => {
    const payload = event.payload;
    const currentPath = $("projectPath").value.trim();
    if (!payload.projectPath || payload.projectPath === currentPath) {
      addLogs([payload]);
      scheduleStats();
    }
  });

  await listen("agent-status", (event) => {
    const payload = event.payload;
    if (payload?.projectPath) {
      projectRunningMap[payload.projectPath] = Boolean(payload.running);
      renderProjectSidebar();
      const currentPath = $("projectPath").value.trim();
      if (payload.projectPath === currentPath) {
        setRunning(Boolean(payload.running));
      }
    } else {
      setRunning(Boolean(payload?.running));
    }
    scheduleStats();
  });

  const currentPath = $("projectPath").value.trim();
  renderProjectSidebar();

  try {
    const status = await invoke("get_status", { projectPath: currentPath || null });
    applyStats(status.stats);
    setRunning(status.running);
    if (status.logs?.length) {
      state.logs = [];
      addLogs(status.logs);
    }
  } catch (error) {
    toast(`Durum okunamadı: ${error}`, "warn");
  }
}

bootstrap();
