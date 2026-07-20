// Passkey/WebAuthn client helpers. Translates between the JSON shapes the
// `webauthn-rs` server emits and the `BufferSource`-based shapes the browser
// WebAuthn API expects.
//
// All binary fields use base64url (RFC 4648 §5: -/_ alphabet, no padding).

function base64urlToBuffer(b64url: string): ArrayBuffer {
  let s = b64url.replace(/-/g, "+").replace(/_/g, "/");
  while (s.length % 4 !== 0) s += "=";
  const binary = atob(s);
  const buf = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    buf[i] = binary.charCodeAt(i);
  }
  return buf.buffer;
}

function bufferToBase64url(buffer: ArrayBuffer): string {
  const bytes = new Uint8Array(buffer);
  let binary = "";
  for (let i = 0; i < bytes.length; i++) {
    binary += String.fromCharCode(bytes[i]);
  }
  return btoa(binary)
    .replace(/\+/g, "-")
    .replace(/\//g, "_")
    .replace(/=+$/g, "");
}

interface CreationOptionsJson {
  publicKey: {
    challenge: string;
    user: { id: string; name: string; displayName: string };
    excludeCredentials?: { id: string; type: string; transports?: string[] }[];
    [key: string]: unknown;
  };
}

function prepareCreationOptions(
  options: CreationOptionsJson,
): PublicKeyCredentialCreationOptions {
  const pk = options.publicKey;
  const out: any = { ...pk };
  out.challenge = base64urlToBuffer(pk.challenge);
  out.user = { ...pk.user, id: base64urlToBuffer(pk.user.id) };
  if (Array.isArray(pk.excludeCredentials)) {
    out.excludeCredentials = pk.excludeCredentials.map((c) => ({
      ...c,
      id: base64urlToBuffer(c.id),
    }));
  }
  return out as PublicKeyCredentialCreationOptions;
}

function prepareCreationResponse(credential: PublicKeyCredential): unknown {
  const response = credential.response as AuthenticatorAttestationResponse;
  const transports =
    typeof (response as any).getTransports === "function"
      ? (response as any).getTransports()
      : undefined;
  return {
    id: credential.id,
    rawId: bufferToBase64url(credential.rawId),
    type: credential.type,
    response: {
      clientDataJSON: bufferToBase64url(response.clientDataJSON),
      attestationObject: bufferToBase64url(response.attestationObject),
      ...(transports ? { transports } : {}),
    },
    extensions: (credential as any).getClientExtensionResults
      ? (credential as any).getClientExtensionResults()
      : {},
  };
}

interface RequestOptionsJson {
  publicKey: {
    challenge: string;
    allowCredentials?: { id: string; type: string; transports?: string[] }[];
    [key: string]: unknown;
  };
}

function prepareAssertionOptions(
  options: RequestOptionsJson,
): PublicKeyCredentialRequestOptions {
  const pk = options.publicKey;
  const out: any = { ...pk };
  out.challenge = base64urlToBuffer(pk.challenge);
  if (Array.isArray(pk.allowCredentials)) {
    out.allowCredentials = pk.allowCredentials.map((c) => ({
      ...c,
      id: base64urlToBuffer(c.id),
    }));
  }
  return out as PublicKeyCredentialRequestOptions;
}

function prepareAssertionResponse(credential: PublicKeyCredential): unknown {
  const response = credential.response as AuthenticatorAssertionResponse;
  return {
    id: credential.id,
    rawId: bufferToBase64url(credential.rawId),
    type: credential.type,
    response: {
      authenticatorData: bufferToBase64url(response.authenticatorData),
      clientDataJSON: bufferToBase64url(response.clientDataJSON),
      signature: bufferToBase64url(response.signature),
      userHandle: response.userHandle
        ? bufferToBase64url(response.userHandle)
        : null,
    },
    extensions: (credential as any).getClientExtensionResults
      ? (credential as any).getClientExtensionResults()
      : {},
  };
}

async function postJson(url: string, body: unknown): Promise<Response> {
  return fetch(url, {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
}

interface RegisterArgs {
  username: string;
  displayName?: string;
}

interface LoginArgs {
  username: string;
}

function createPasskeyClient(basePath: string = "") {
  return {
    async register({ username, displayName }: RegisterArgs): Promise<unknown> {
      const startResp = await postJson(`${basePath}/register/start`, {
        username,
        display_name: displayName,
      });
      if (!startResp.ok) {
        throw new Error(`register/start failed: ${startResp.status}`);
      }
      const startJson = (await startResp.json()) as CreationOptionsJson;
      const options = prepareCreationOptions(startJson);
      const credential = (await navigator.credentials.create({
        publicKey: options,
      })) as PublicKeyCredential | null;
      if (!credential) throw new Error("navigator.credentials.create returned null");
      const finishBody = prepareCreationResponse(credential);
      const finishResp = await postJson(`${basePath}/register/finish`, finishBody);
      if (!finishResp.ok) {
        throw new Error(`register/finish failed: ${finishResp.status}`);
      }
      return finishResp.json().catch(() => ({}));
    },

    async login({ username }: LoginArgs): Promise<unknown> {
      const startResp = await postJson(`${basePath}/auth/start`, { username });
      if (!startResp.ok) {
        throw new Error(`auth/start failed: ${startResp.status}`);
      }
      const startJson = (await startResp.json()) as RequestOptionsJson;
      const options = prepareAssertionOptions(startJson);
      const credential = (await navigator.credentials.get({
        publicKey: options,
      })) as PublicKeyCredential | null;
      if (!credential) throw new Error("navigator.credentials.get returned null");
      const finishBody = prepareAssertionResponse(credential);
      const finishResp = await postJson(`${basePath}/auth/finish`, finishBody);
      if (!finishResp.ok) {
        throw new Error(`auth/finish failed: ${finishResp.status}`);
      }
      return finishResp.json().catch(() => ({}));
    },
  };
}

export { createPasskeyClient };
