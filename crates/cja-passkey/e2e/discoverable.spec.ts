import { expect, test, chromium, type BrowserContext, type CDPSession, type Page } from "@playwright/test";

const baseURL = process.env.CJA_PASSKEY_TEST_BASE_URL;
if (!baseURL) throw new Error("CJA_PASSKEY_TEST_BASE_URL is required");

interface Snapshot {
  session_id: string;
  user_id: string | null;
  challenge: { type: string } | null;
  credentials: Array<{
    username: string;
    user_id: string;
    credential_id_pk: string;
    credential_json: unknown;
    last_used_at: string | null;
  }>;
}

interface Assertion {
  id: string;
  rawId: string;
  type: string;
  response: {
    authenticatorData: string;
    clientDataJSON: string;
    signature: string;
    userHandle: string | null;
  };
  extensions: AuthenticationExtensionsClientOutputs;
}

async function authenticator(context: BrowserContext, page: Page): Promise<{ cdp: CDPSession; id: string }> {
  const cdp = await context.newCDPSession(page);
  await cdp.send("WebAuthn.enable");
  const { authenticatorId: id } = await cdp.send("WebAuthn.addVirtualAuthenticator", {
    options: {
      protocol: "ctap2",
      transport: "internal",
      hasResidentKey: true,
      hasUserVerification: true,
      isUserVerified: true,
    },
  });
  return { cdp, id };
}

async function snapshot(page: Page): Promise<Snapshot> {
  return page.evaluate(async () => (await fetch("/test/state")).json());
}

async function me(page: Page): Promise<{ status: number; body: string }> {
  return page.evaluate(async () => {
    const response = await fetch("/me");
    return { status: response.status, body: await response.text() };
  });
}

async function mutate(page: Page, body: object): Promise<void> {
  const status = await page.evaluate(async (payload) => {
    const response = await fetch("/test/mutate", {
      method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(payload),
    });
    return response.status;
  }, body);
  expect(status).toBe(204);
}

async function clickClient(page: Page, button: "register" | "login" | "named", username?: string): Promise<void> {
  await page.evaluate((name) => {
    (window as Window & { username?: string; outcome?: unknown }).username = name;
    (window as Window & { outcome?: unknown }).outcome = undefined;
  }, username);
  await page.locator(`#${button}`).click();
  await expect.poll(() => page.evaluate(() => (window as Window & { outcome?: unknown }).outcome)).not.toBeUndefined();
  const result = await page.evaluate(() => (window as Window & { outcome?: { ok: boolean; error?: string } }).outcome);
  expect(result).toBeDefined();
  expect(result!.ok, result!.error).toBe(true);
}

async function assertion(page: Page): Promise<{ body: Assertion; start: { status: number; cookie: string | null } }> {
  return page.evaluate(async () => {
    const start = await fetch("/auth/discoverable/start", {
      method: "POST", headers: { "Content-Type": "application/json" }, body: "{}",
    });
    const challenge = await start.json();
    const b64ToBuffer = (value: string): ArrayBuffer => {
      const normalized = value.replace(/-/g, "+").replace(/_/g, "/");
      const bytes = atob(normalized.padEnd(Math.ceil(normalized.length / 4) * 4, "="));
      return Uint8Array.from(bytes, (ch) => ch.charCodeAt(0)).buffer;
    };
    const toB64 = (buffer: ArrayBuffer): string =>
      btoa(String.fromCharCode(...new Uint8Array(buffer))).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/g, "");
    const options = challenge.publicKey as PublicKeyCredentialRequestOptions & { challenge: string };
    const credential = await navigator.credentials.get({
      publicKey: { ...options, challenge: b64ToBuffer(options.challenge) },
    }) as PublicKeyCredential;
    const response = credential.response as AuthenticatorAssertionResponse;
    return {
      start: { status: start.status, cookie: document.cookie || null },
      body: {
        id: credential.id, rawId: toB64(credential.rawId), type: credential.type,
        response: {
          authenticatorData: toB64(response.authenticatorData),
          clientDataJSON: toB64(response.clientDataJSON),
          signature: toB64(response.signature),
          userHandle: response.userHandle ? toB64(response.userHandle) : null,
        },
        extensions: credential.getClientExtensionResults(),
      },
    };
  });
}

async function finish(page: Page, body: Assertion): Promise<number> {
  return page.evaluate(async (payload) => (await fetch("/auth/discoverable/finish", {
    method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(payload),
  })).status, body);
}

test("resident passkeys authenticate without a username and reject forged ceremony paths", async () => {
  const browser = await chromium.launch();
  try {
    const contextA = await browser.newContext({ baseURL });
    const pageA = await contextA.newPage();
    const authA = await authenticator(contextA, pageA);
    await pageA.goto("/");
    await clickClient(pageA, "register", "alice");
    const registered = await snapshot(pageA);
    const alice = registered.credentials.find((c) => c.username === "alice");
    expect(alice).toBeDefined();
    expect(registered.user_id).toBe(alice!.user_id);

    // Named fallback uses the original authenticator before cloning it.
    await contextA.clearCookies();
    await pageA.reload();
    await clickClient(pageA, "named", "alice");
    expect((await me(pageA))).toEqual({ status: 200, body: "alice" });
    const named = await snapshot(pageA);
    expect(named.challenge).toBeNull();
    expect(named.credentials.find((c) => c.username === "alice")?.last_used_at).not.toBeNull();

    // A new context receives a copy of the same resident key. Its first cookie
    // creating request is the discoverable start POST.
    const { credentials } = await authA.cdp.send("WebAuthn.getCredentials", { authenticatorId: authA.id });
    expect(credentials).toHaveLength(1);
    expect(credentials[0].isResidentCredential).toBe(true);
    const fresh = await browser.newContext({ baseURL });
    const freshPage = await fresh.newPage();
    const freshAuth = await authenticator(fresh, freshPage);
    await freshAuth.cdp.send("WebAuthn.addCredential", { authenticatorId: freshAuth.id, credential: credentials[0] });
    const startResponse = await fresh.request.post("/auth/discoverable/start", { data: {} });
    expect(startResponse.status()).toBe(200);
    expect((await startResponse.json()).publicKey.allowCredentials).toEqual([]);
    const cookie = startResponse.headers()["set-cookie"];
    expect(cookie).toContain("Path=/");
    await freshPage.goto("/");
    const before = await snapshot(freshPage);
    expect(before.user_id).toBeNull();
    await clickClient(freshPage, "login");
    const after = await snapshot(freshPage);
    expect(after.session_id).toBe(before.session_id);
    expect(after.challenge).toBeNull();
    expect(after.credentials.find((c) => c.username === "alice")?.last_used_at).not.toBeNull();
    expect(after.credentials.find((c) => c.username === "alice")?.last_used_at)
      .not.toBe(named.credentials.find((c) => c.username === "alice")?.last_used_at);
    expect(after.credentials.find((c) => c.username === "alice")?.credential_json).toBeTruthy();
    expect((await me(freshPage))).toEqual({ status: 200, body: "alice" });
    await contextA.close();

    await fresh.clearCookies();
    await freshPage.reload();
    const valid = await assertion(freshPage);
    expect(valid.start.status).toBe(200);
    const handle = valid.body.response.userHandle;
    expect(handle).not.toBeNull();
    expect(Buffer.from(handle!.replace(/-/g, "+").replace(/_/g, "/"), "base64").toString("hex"))
      .toBe(alice!.user_id.replace(/-/g, ""));
    const sessionBefore = await snapshot(freshPage);
    expect(sessionBefore.user_id).toBeNull();
    const missingHandle: Assertion = structuredClone(valid.body);
    missingHandle.response.userHandle = null;
    expect(await finish(freshPage, missingHandle)).toBe(401);
    const unknownHandle: Assertion = structuredClone(valid.body);
    unknownHandle.response.userHandle = "AAAAAAAAAAAAAAAAAAAAAA";
    expect(await finish(freshPage, unknownHandle)).toBe(401);
    const unknownCredential: Assertion = structuredClone(valid.body);
    unknownCredential.rawId = "AQ";
    unknownCredential.id = "AQ";
    expect(await finish(freshPage, unknownCredential)).toBe(401);
    const invalidSignature: Assertion = structuredClone(valid.body);
    invalidSignature.response.signature = "AQ";
    expect(await finish(freshPage, invalidSignature)).toBe(401);
    expect((await me(freshPage)).status).toBe(401);
    expect(await finish(freshPage, valid.body)).toBe(200);
    expect(await finish(freshPage, valid.body)).toBe(400); // sequential replay
    expect((await snapshot(freshPage)).session_id).toBe(sessionBefore.session_id);

    await fresh.clearCookies();
    await freshPage.reload();
    const wrongSession = await assertion(freshPage);
    const other = await browser.newContext({ baseURL });
    const otherPage = await other.newPage();
    await otherPage.goto("/");
    await snapshot(otherPage);
    expect(await finish(otherPage, wrongSession.body)).toBe(400);
    expect((await me(otherPage)).status).toBe(401);
    await other.close();
    await assertion(freshPage); // overwrite the saved challenge
    expect(await finish(freshPage, wrongSession.body)).toBe(401);
    expect((await me(freshPage)).status).toBe(401);

    // A genuine assertion with a user handle whose user and credential were deleted.
    await fresh.clearCookies();
    await freshPage.reload();
    const deleted = await assertion(freshPage);
    await mutate(freshPage, { action: "delete_user", username: "alice" });
    expect(await finish(freshPage, deleted.body)).toBe(401);
    expect((await me(freshPage)).status).toBe(401);
    await fresh.close();

    const contextB = await browser.newContext({ baseURL });
    const pageB = await contextB.newPage();
    await authenticator(contextB, pageB);
    await pageB.goto("/");
    await clickClient(pageB, "register", "bob");
    const contextC = await browser.newContext({ baseURL });
    const pageC = await contextC.newPage();
    await authenticator(contextC, pageC);
    await pageC.goto("/");
    await clickClient(pageC, "register", "carol");
    await contextB.clearCookies();
    await pageB.reload();
    const mismatched = await assertion(pageB);
    await mutate(pageB, { action: "change_owner", credential_owner: "bob", new_owner: "carol" });
    expect(await finish(pageB, mismatched.body)).toBe(401);
    expect((await me(pageB)).status).toBe(401);
    await contextB.close();
    await contextC.close();
  } finally {
    await browser.close();
  }
});
