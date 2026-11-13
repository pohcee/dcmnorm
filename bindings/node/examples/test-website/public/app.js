const form = document.getElementById("upload-form");
const fileInput = document.getElementById("file-input");
const submitBtn = document.getElementById("submit-btn");
const statusEl = document.getElementById("status");
const resultsEl = document.getElementById("results");
const timingsEl = document.getElementById("timings");
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
  timingsEl.innerHTML = "";
  // [label, milliseconds, the binding call it times (shown as a tooltip)]
  const rows = [
    ["Validate", timings.validateMs, "checkDicom"],
    ["Metadata", timings.metadataMs, "readJson"],
  ];
  if (hasPixelData) {
    rows.push(
      ["Render", timings.renderTotalMs, "renderFrame, every frame"],
      ["Per frame", timings.renderAvgMs, "average renderFrame time"],
    );
  }
  rows.push(["Total", timings.totalMs, "the whole request"]);
  for (const [label, ms, call] of rows) {
    const item = document.createElement("div");
    item.title = call;
    if (label === "Total") item.className = "total";
    const dt = document.createElement("dt");
    dt.textContent = label;
    const dd = document.createElement("dd");
    dd.textContent = formatMs(ms);
    item.append(dt, dd);
    timingsEl.append(item);
  }
}

function renderFrames(frames) {
  framesGridEl.innerHTML = "";
  for (const frame of frames) {
    const card = document.createElement("figure");
    card.className = "frame-card";

    const thumbBtn = document.createElement("button");
    thumbBtn.type = "button";
    thumbBtn.className = "frame-thumb-btn";
    thumbBtn.style.aspectRatio = `${frame.width} / ${frame.height}`;
    thumbBtn.title = `View frame ${frame.index + 1} larger`;
    thumbBtn.addEventListener("click", () => openLightbox(frame.index));

    const img = document.createElement("img");
    img.src = frame.dataUrl;
    img.alt = `Frame ${frame.index + 1}`;
    thumbBtn.append(img);

    const meta = document.createElement("figcaption");
    meta.className = "frame-meta";
    for (const text of [`${frame.index + 1}`, `${frame.width}×${frame.height}`, formatMs(frame.timeMs)]) {
      const part = document.createElement("span");
      part.textContent = text;
      meta.append(part);
    }

    card.append(thumbBtn, meta);
    framesGridEl.append(card);
  }
}

function showResult(body) {
  currentSource =
    body.kind === "upload" ? { kind: "upload", key: body.uploadId } : { kind: "sample", key: body.fileName };

  setStatus(`Showing ${body.fileName}, ${formatBytes(body.fileSizeBytes)}`);
  renderTimings(body.timings, body.hasPixelData);

  frameCountEl.textContent =
    body.framesRendered === body.numberOfFrames ? `${body.numberOfFrames}` : `${body.framesRendered} of ${body.numberOfFrames}`;
  noPixelDataNoteEl.hidden = body.hasPixelData;
  framesHintEl.hidden = !body.hasPixelData;
  truncatedNoteEl.hidden = !body.truncated;
  if (body.truncated) {
    truncatedNoteEl.textContent = `Showing the first ${body.framesRendered} of ${body.numberOfFrames} frames: this server renders at most ${body.framesRendered} per file.`;
  }

  renderFrames(body.frames);
  metadataJsonEl.textContent = JSON.stringify(body.metadata, null, 2);

  resultsEl.hidden = false;
  const reduceMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
  resultsEl.scrollIntoView({ behavior: reduceMotion ? "auto" : "smooth", block: "start" });
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
  lightboxCaptionEl.textContent = `Rendering frame ${frameIndex + 1}...`;

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
    lightboxImgEl.alt = `Frame ${frameIndex + 1}`;
    lightboxCaptionEl.textContent = `Frame ${frameIndex + 1}, ${frame.width}×${frame.height}, rendered in ${formatMs(frame.timeMs)}`;
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

function processFile(file) {
  if (!file) return;
  if (!file.name.toLowerCase().endsWith(".dcm")) {
    setStatus(`${file.name} isn't a .dcm file. Choose a DICOM file with the .dcm extension.`, true);
    return;
  }

  const formData = new FormData();
  formData.append("dicomFile", file);

  runProcessRequest(
    () => fetch("/api/process", { method: "POST", body: formData }),
    `Processing ${file.name}...`,
  );
}

// The file is processed as soon as it's chosen or dropped: there is nothing else to fill in.
submitBtn.addEventListener("click", () => fileInput.click());
fileInput.addEventListener("change", () => {
  processFile(fileInput.files[0]);
  fileInput.value = ""; // choosing the same file again processes it again
});
form.addEventListener("submit", (event) => event.preventDefault());

form.addEventListener("dragover", (event) => {
  event.preventDefault();
  form.classList.add("is-over");
});
form.addEventListener("dragleave", (event) => {
  if (!form.contains(event.relatedTarget)) form.classList.remove("is-over");
});
form.addEventListener("drop", (event) => {
  event.preventDefault();
  form.classList.remove("is-over");
  if (submitBtn.disabled) return;
  processFile(event.dataTransfer.files[0]);
});

async function loadSamples() {
  try {
    const response = await fetch("/api/samples");
    const samples = await response.json();

    samplesListEl.innerHTML = "";

    if (!Array.isArray(samples) || samples.length === 0) {
      samplesListEl.textContent = "This server has no sample files.";
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
          `Processing ${sample.name}...`,
        );
      });

      samplesListEl.append(row);
    }
  } catch (err) {
    samplesListEl.textContent = `The sample list didn't load (${err.message || err}). Reload the page to try again.`;
  }
}

async function loadVersion() {
  try {
    const response = await fetch("/api/version");
    const { dcmnormVersion, dcmnormNodeVersion } = await response.json();

    if (!dcmnormVersion) throw new Error("no version");
    const text = dcmnormNodeVersion
      ? `dcmnorm ${dcmnormVersion} with dcmnorm-node ${dcmnormNodeVersion}`
      : `dcmnorm ${dcmnormVersion}`;
    versionLineEl.textContent = text;
    versionLineAboutEl.textContent = text;
  } catch {
    versionLineEl.textContent = "";
    versionLineAboutEl.textContent = "dcmnorm";
  }
}

loadSamples();
loadVersion();
