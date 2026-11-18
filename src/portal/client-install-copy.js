for (const button of document.querySelectorAll("[data-install-copy]")) {
  const command = document.getElementById(button.dataset.installCopy);
  const status = button.nextElementSibling;
  button.hidden = false;

  button.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(command.textContent);
      status.textContent = "已复制安装命令";
    } catch {
      const selection = window.getSelection();
      const range = document.createRange();
      range.selectNodeContents(command);
      selection.removeAllRanges();
      selection.addRange(range);
      status.textContent = "已选中命令，请手动复制";
    }
  });
}
