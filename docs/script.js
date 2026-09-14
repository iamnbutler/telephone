for (const button of document.querySelectorAll('[data-copy]')) {
  button.hidden = false;
  button.addEventListener('click', async () => {
    const code = document.getElementById(button.dataset.copy);
    const status = document.querySelector('#copy-status');
    try {
      await navigator.clipboard.writeText(code.textContent);
      button.textContent = 'Copied!';
      status.textContent = 'Setup commands copied to clipboard.';
    } catch {
      const range = document.createRange();
      range.selectNodeContents(code);
      const selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      button.textContent = 'Code selected';
      status.textContent = 'Press Control C or Command C to copy the selected commands.';
    }
    setTimeout(() => { button.textContent = 'Copy commands'; }, 2500);
  });
}
