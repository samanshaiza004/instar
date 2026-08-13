document.addEventListener("click", async (event) => {
  const button = event.target.closest("[data-copy]");
  if (!button) return;
  const original = button.textContent;
  try {
    await navigator.clipboard.writeText(button.dataset.copy);
    button.textContent = "COPIED";
  } catch {
    button.textContent = "SELECT";
  }
  window.setTimeout(() => { button.textContent = original; }, 1400);
});
