// 由 Rust 侧更新加载状态（如首次运行的解压进度提示）。
window.__setStatus = function (text) {
  const element = document.getElementById("status");
  if (element) element.textContent = text;
};

window.__setProgress = function (value, text) {
  const progress = Math.max(0, Math.min(100, Math.round(Number(value) || 0)));
  const container = document.getElementById("progress");
  const bar = document.getElementById("progress-bar");
  const label = document.getElementById("progress-value");
  container.setAttribute("aria-valuenow", String(progress));
  bar.style.width = `${progress}%`;
  label.textContent = `${progress}%`;
  if (text) window.__setStatus(text);
};

// 由 Rust 侧在启动失败时调用。
window.__showError = function (text) {
  document.getElementById("spinner").style.display = "none";
  document.getElementById("progress").style.display = "none";
  document.getElementById("progress-value").style.display = "none";
  document.getElementById("status").style.display = "none";
  const element = document.getElementById("error");
  element.textContent = text;
  element.style.display = "block";
};
