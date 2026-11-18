const button = document.querySelector("#copy-client-command");
const command = document.querySelector("#client-command");
const status = document.querySelector("#copy-command-status");

if (button && command && status) {
  button.hidden = false;
  button.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(command.textContent);
      status.textContent = "已复制命令";
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
