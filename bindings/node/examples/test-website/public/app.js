const form = document.getElementById("upload-form");
const fileInput = document.getElementById("file-input");
const submitBtn = document.getElementById("submit-btn");
const statusEl = document.getElementById("status");
const resultsEl = document.getElementById("results");
const timingsTableBody = document.querySelector("#timings-table tbody");
const frameCountEl = document.getElementById("frame-count");
const truncatedNoteEl = document.getElementById("truncated-note");
const noPixelDataNoteEl = document.getElementById("no-pixel-data-note");
const framesHintEl = document.getElementById("frames-hint");
const framesGridEl = document.getElementById("frames-grid");
const metadataJsonEl = document.getElementById("metadata-json");
const samplesListEl = document.getElementById("samples-list");
const versionLineEl = document.getElementById("version-line");
const versionLineAboutEl = document.getElementById("version-line-about");
const lightboxEl = document.getElementById("lightbox");
const lightboxImgEl = document.getElementById("lightbox-img");
const lightboxCaptionEl = document.getElementById("lightbox-caption");
const lightboxCloseBtn = document.getElementById("lightbox-close");

// Which file the currently-displayed frames came from, so a thumbnail click knows where to
// fetch a larger render of that same frame from - set by showResult() below.
let currentSource = null;

for (const tabBtn of document.querySelectorAll(".tab-btn")) {
  tabBtn.addEventListener("click", () => {
    for (const btn of document.querySelectorAll(".tab-btn")) {
      btn.classList.toggle("active", btn === tabBtn);
    }
    for (const panel of document.querySelectorAll(".tab-panel")) {
      panel.hidden = panel.id !== `tab-${tabBtn.dataset.tab}`;
    }
  });
}

function setStatus(message, isError) {
  statusEl.textContent = message;
  statusEl.classList.toggle("error", Boolean(isError));
}

function formatMs(ms) {
  return `${ms.toFixed(1)} ms`;
}

function formatBytes(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unitIndex = 0;
  while (value >= 1024 && unitIndex < units.length - 1) {
    value /= 1024;
    unitIndex++;
  }
  return `${value.toFixed(1)} ${units[unitIndex]}`;
}

function renderTimings(timings, hasPixelData) {
  timingsTableBody.innerHTML = "";
  const rows = [
    ["Validate (checkDicom)", timings.validateMs],
    ["Read metadata (readJson)", timings.metadataMs],
  ];
  if (hasPixelData) {
    rows.push(
      ["Render all frames (renderFrame x N)", timings.renderTotalMs],
      ["Average per frame", timings.renderAvgMs],
    );
  }
  rows.push(["Total", timings.totalMs]);
  for (const [label, ms] of rows) {
    const tr = document.createElement("tr");
    const tdLabel = document.createElement("td");
    tdLabel.textContent = label;
    const tdValue = document.createElement("td");
    tdValue.textContent = formatMs(ms);
    tr.append(tdLabel, tdValue);
    timingsTableBody.append(tr);
  }
}

function renderFrames(frames) {
  framesGridEl.innerHTML = "";
  for (const frame of frames) {
    const card = document.createElement("div");
    card.className = "frame-card";

    const thumbBtn = document.createElement("button");
    thumbBtn.type = "button";
    thumbBtn.className = "frame-thumb-btn";
    thumbBtn.style.aspectRatio = `${frame.width} / ${frame.height}`;
    thumbBtn.title = "Click to view a larger render (up to 1024x1024)";
    thumbBtn.addEventListener("click", () => openLightbox(frame.index));

    const img = document.createElement("img");
    img.src = frame.dataUrl;
    img.alt = `Frame ${frame.index}`;
    thumbBtn.append(img);

    const meta = document.createElement("div");
    meta.className = "frame-meta";
    meta.textContent = `#${frame.index} · ${frame.width}x${frame.height} · ${formatMs(frame.timeMs)}`;

    card.append(thumbBtn, meta);
    framesGridEl.append(card);
  }
}

function showResult(body) {
  currentSource =
    body.kind === "upload" ? { kind: "upload", key: body.uploadId } : { kind: "sample", key: body.fileName };

  setStatus(`Processed ${body.fileName} (${body.fileSizeBytes.toLocaleString()} bytes)`);
  renderTimings(body.timings, body.hasPixelData);

  frameCountEl.textContent = `${body.framesRendered} of ${body.numberOfFrames}`;
  noPixelDataNoteEl.hidden = body.hasPixelData;
  framesHintEl.hidden = !body.hasPixelData;
  truncatedNoteEl.hidden = !body.truncated;
  if (body.truncated) {
    truncatedNoteEl.textContent = `Only the first ${body.framesRendered} of ${body.numberOfFrames} frames were rendered (server MAX_FRAMES_RENDERED limit).`;
  }

  renderFrames(body.frames);
  metadataJsonEl.textContent = JSON.stringify(body.metadata, null, 2);

  resultsEl.hidden = false;
}

function closeLightbox() {
  lightboxEl.hidden = true;
  lightboxImgEl.src = "";
}

lightboxCloseBtn.addEventListener("click", closeLightbox);
lightboxEl.addEventListener("click", (event) => {
  if (event.target === lightboxEl) closeLightbox();
});
document.addEventListener("keydown", (event) => {
  if (event.key === "Escape" && !lightboxEl.hidden) closeLightbox();
});

async function openLightbox(frameIndex) {
  if (!currentSource) return;

  lightboxEl.hidden = false;
  lightboxImgEl.src = "";
  lightboxCaptionEl.textContent = "Rendering full-resolution frame...";

  const base =
    currentSource.kind === "upload"
      ? `/api/uploads/${encodeURIComponent(currentSource.key)}/frames/${frameIndex}`
      : `/api/samples/${encodeURIComponent(currentSource.key)}/frames/${frameIndex}`;

  try {
    const response = await fetch(`${base}?maxDim=1024`);
    const frame = await response.json();

    if (!response.ok) {
      lightboxCaptionEl.textContent = frame.error || `Request failed with status ${response.status}`;
      return;
    }

    lightboxImgEl.src = frame.dataUrl;
    lightboxCaptionEl.textContent = `Frame #${frameIndex} · ${frame.width}x${frame.height} · rendered in ${formatMs(frame.timeMs)}`;
  } catch (err) {
    lightboxCaptionEl.textContent = err.message || String(err);
  }
}

function setBusy(isBusy) {
  submitBtn.disabled = isBusy;
  for (const btn of samplesListEl.querySelectorAll("button")) {
    btn.disabled = isBusy;
  }
}

async function runProcessRequest(requestFn, busyMessage) {
  setBusy(true);
  resultsEl.hidden = true;
  closeLightbox();
  setStatus(busyMessage);

  try {
    const response = await requestFn();
    const body = await response.json();

    if (!response.ok) {
      setStatus(body.error || `Request failed with status ${response.status}`, true);
      return;
    }

    showResult(body);
  } catch (err) {
    setStatus(err.message || String(err), true);
  } finally {
    setBusy(false);
  }
}

form.addEventListener("submit", (event) => {
  event.preventDefault();

  const file = fileInput.files[0];
  if (!file) return;

  if (!file.name.toLowerCase().endsWith(".dcm")) {
    setStatus("Please select a file with a .dcm extension.", true);
    return;
  }

  const formData = new FormData();
  formData.append("dicomFile", file);

  runProcessRequest(
    () => fetch("/api/process", { method: "POST", body: formData }),
    `Uploading ${file.name}...`,
  );
});

async function loadSamples() {
  try {
    const response = await fetch("/api/samples");
    const samples = await response.json();

    samplesListEl.innerHTML = "";

    if (!Array.isArray(samples) || samples.length === 0) {
      samplesListEl.textContent = "No bundled sample files are available.";
      return;
    }

    for (const sample of samples) {
      const row = document.createElement("button");
      row.type = "button";
      row.className = "sample-row";

      const header = document.createElement("div");
      header.className = "sample-header";

      const name = document.createElement("span");
      name.className = "sample-name";
      name.textContent = sample.name;

      const size = document.createElement("span");
      size.className = "sample-size";
      size.textContent = formatBytes(sample.sizeBytes);

      header.append(name, size);

      const description = document.createElement("span");
      description.className = "sample-description";
      description.textContent = sample.description;

      row.append(header, description);
      row.addEventListener("click", () => {
        runProcessRequest(
          () => fetch(`/api/samples/${encodeURIComponent(sample.name)}/process`, { method: "POST" }),
          `Processing sample ${sample.name}...`,
        );
      });

      samplesListEl.append(row);
    }
  } catch (err) {
    samplesListEl.textContent = `Failed to load sample files: ${err.message || err}`;
  }
}

async function loadVersion() {
  try {
    const response = await fetch("/api/version");
    const { dcmnormVersion, dcmnormNodeVersion } = await response.json();

    const parts = [];
    if (dcmnormVersion) parts.push(`dcmnorm v${dcmnormVersion}`);
    if (dcmnormNodeVersion) parts.push(`dcmnorm-node v${dcmnormNodeVersion}`);
    const text = parts.length > 0 ? `Testing ${parts.join(" · ")}` : "";

    versionLineEl.textContent = text;
    versionLineAboutEl.textContent = parts.join(" · ") || "an unknown dcmnorm version";
  } catch {
    versionLineEl.textContent = "";
    versionLineAboutEl.textContent = "an unknown dcmnorm version";
  }
}

loadSamples();
loadVersion();
