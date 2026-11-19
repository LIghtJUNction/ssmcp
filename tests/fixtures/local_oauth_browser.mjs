// Real browser half of e2e_local.py. Secrets travel through this process's
// private stdin/stdout, never argv, diagnostics, screenshots or a saved profile.
import { pathToFileURL } from "node:url";

let browser;
try {
  let source = "";
  process.stdin.setEncoding("utf8");
  for await (const chunk of process.stdin) {
    source += chunk;
    if (source.length > 16384) throw new Error("input bound");
  }
  const input = JSON.parse(source);
  const origin = new URL(input.origin);
  const authorize = new URL(input.authorize_url);
  const callback = new URL(input.callback_url);
  if (origin.protocol !== "http:" || origin.hostname !== "127.0.0.1"
      || authorize.origin !== origin.origin || authorize.pathname !== "/oauth/authorize"
      || callback.protocol !== "http:" || callback.hostname !== "127.0.0.1"
      || !/^[0-9a-f]{64}$/.test(input.login_code)) throw new Error("input scope");
  const moduleName = process.env.SSMCP_E2E_PLAYWRIGHT_MODULE || "playwright";
  const imported = await import(moduleName.startsWith("/") ? pathToFileURL(moduleName).href : moduleName);
  const playwright = imported.chromium ? imported : imported.default;
  browser = await playwright.chromium.launch({
    headless: true,
    executablePath: process.env.SSMCP_E2E_CHROMIUM || "chromium",
    args: ["--disable-dev-shm-usage"],
  });
  const context = await browser.newContext();
  const page = await context.newPage();
  page.setDefaultTimeout(10000);
  await page.goto(authorize.href, { waitUntil: "domcontentloaded" });
  const cookies = await context.cookies(origin.href);
  const cookie = cookies.find((item) => item.name === "ssmcp-local-oauth");
  if (!cookie || cookie.secure || !cookie.httpOnly || cookie.sameSite !== "Lax") {
    throw new Error("loopback cookie rejected");
  }
  await page.locator("#login_code").fill(input.login_code);
  await Promise.all([
    page.waitForURL((url) => url.origin === callback.origin && url.pathname === callback.pathname),
    page.getByRole("button", { name: "使用登录码授权", exact: true }).click(),
  ]);
  process.stdout.write(JSON.stringify({
    callback_url: page.url(),
    cookie: { name: cookie.name, secure: cookie.secure, httpOnly: cookie.httpOnly, sameSite: cookie.sameSite },
  }) + "\n");
} catch {
  // Browser errors may contain the authorization URL. Do not forward them.
  process.stderr.write("Isolated local browser authorization failed.\n");
  process.exitCode = 1;
} finally {
  if (browser) await browser.close();
}
