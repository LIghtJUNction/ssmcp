// The manager can launch fixed owner actions. The browser that receives the
// private loopback URL has no access to the manager's shell bridge.
(() => {
  const status = document.getElementById('status');
  const open = document.getElementById('open');
  const setup = document.getElementById('setup');
  const zuse = document.getElementById('zuse');
  let sequence = 0;
  const launch = (command) => {
    if (open.disabled) return;
    if (!window.ksu || typeof window.ksu.exec !== 'function') {
      status.textContent = '请返回模块管理器，点「操作」，选择「打开工作台」。';
      return;
    }
    open.disabled = setup.disabled = zuse.disabled = true;
    status.textContent = '正在打开本机页面…';
    const callback = '__ssmcpOpen' + ++sequence;
    const finish = (message) => {
      delete window[callback];
      clearTimeout(timer);
      open.disabled = setup.disabled = zuse.disabled = false;
      status.textContent = message;
    };
    const timer = setTimeout(() => finish('未收到启动回执。请查看系统浏览器或模块状态。'), 25000);
    window[callback] = code => finish(Number(code) === 0
      ? '本机页面已在系统浏览器中打开。'
      : '打开失败，请检查模块是否已启用。');
    try {
      window.ksu.exec(command, '{}', callback);
    } catch {
      finish('模块管理器无法打开页面，请使用模块的「操作」入口。');
    }
  };
  const gui = '/system/bin/sh /data/adb/modules/ssmcp/open.sh';
  open.onclick = () => launch(gui);
  setup.onclick = () => launch('/system/bin/sh /data/adb/modules/ssmcp/onboarding/open.sh');
  zuse.onclick = () => launch('/system/bin/am start -a android.intent.action.VIEW -d http://127.0.0.1:43497/');
  launch(gui);
})();
