"use strict";
// Generates App Store provisioning profiles via the App Store Connect API.
// Used by build.cjs ("generate" builds) and the "Generate a provisioning
// profile" operation in index.js.

const { createHash, createPrivateKey, sign } = require("crypto");

const ASC = "https://api.appstoreconnect.apple.com";

/** The .p8 key's bytes, or null. Survives lost line breaks from pasting. */
function keyDer(pem) {
  const body = String(pem || "")
    .replace(/-----[A-Z ]+-----/g, "")
    .replace(/\s+/g, "");
  return body ? Buffer.from(body, "base64") : null;
}

/** SHA-1 as `security find-identity` prints it. */
const sha1Of = (base64) =>
  createHash("sha1")
    .update(Buffer.from(base64, "base64"))
    .digest("hex")
    .toUpperCase();

/** Fixed name, so the profile can be found again. */
const profileName = (bundleId) => "Feldspar " + bundleId + " App Store";

class IosProfileGenerator {
  /** env: FELDSPAR_ASC_* and FELDSPAR_IOS_IDENTITY. `api` replaces the HTTP
   * calls in tests. Throws if the key is incomplete. */
  constructor(env, { api, now = new Date(), log = console.log } = {}) {
    this.issuer = String(env.FELDSPAR_ASC_ISSUER_ID || "").trim();
    this.keyId = String(env.FELDSPAR_ASC_KEY_ID || "").trim();
    this.key = keyDer(env.FELDSPAR_ASC_KEY);
    const missing = [
      [this.issuer, "issuer ID"],
      [this.keyId, "key ID"],
      [this.key, "private key"],
    ]
      .filter(([value]) => !value)
      .map(([, label]) => label);
    if (missing.length) {
      throw new Error(
        "A generated provisioning profile needs the App Store Connect API key's " +
          missing.join(", ") +
          ': under Settings → Modules → React Native, switch on "Use the App Store Connect API" and ' +
          "fill them in."
      );
    }
    this.wanted = String(env.FELDSPAR_IOS_IDENTITY || "").trim();
    this.api =
      api || ((method, apiPath, body) => this.request(method, apiPath, body));
    this.now = now;
    this.log = log;
    this.bearer = null;
  }

  /** ES256 JWT, valid 15 minutes (Apple allows 20). */
  token(nowSeconds) {
    const b64url = (data) => Buffer.from(data).toString("base64url");
    const header = b64url(
      JSON.stringify({ alg: "ES256", kid: this.keyId, typ: "JWT" })
    );
    const claims = b64url(
      JSON.stringify({
        iss: this.issuer,
        iat: nowSeconds,
        exp: nowSeconds + 900,
        aud: "appstoreconnect-v1",
      })
    );
    let key;
    try {
      key = createPrivateKey({ key: this.key, format: "der", type: "pkcs8" });
    } catch (e) {
      throw new Error(
        "The App Store Connect API private key is not a valid .p8 key: " +
          e.message
      );
    }
    // JWT wants raw r|s, not DER.
    const signature = sign("sha256", Buffer.from(header + "." + claims), {
      key,
      dsaEncoding: "ieee-p1363",
    });
    return header + "." + claims + "." + b64url(signature);
  }

  /** One API call; returns JSON or null. */
  async request(method, apiPath, body) {
    this.bearer = this.bearer || this.token(Math.floor(Date.now() / 1000));
    const res = await fetch(ASC + apiPath, {
      method,
      headers: {
        Authorization: "Bearer " + this.bearer,
        "Content-Type": "application/json",
      },
      body: body ? JSON.stringify(body) : undefined,
    });
    const text = await res.text();
    const json = text ? JSON.parse(text) : null;
    if (!res.ok) {
      const why = ((json && json.errors) || [])
        .map((e) => e.detail || e.title)
        .join("; ");
      throw new Error(
        "App Store Connect refused " +
          method +
          " " +
          apiPath +
          " (" +
          res.status +
          "): " +
          (why || text)
      );
    }
    return json;
  }

  /** An unexpired distribution certificate. With `identities` (keychain) it
   * must also be there; null accepts any. FELDSPAR_IOS_IDENTITY picks one of
   * several. */
  pickCertificate(certificates, identities) {
    const usable = [];
    for (const cert of certificates) {
      const attrs = cert.attributes || {};
      if (!attrs.certificateContent) continue;
      if (
        attrs.expirationDate &&
        new Date(attrs.expirationDate).getTime() <= this.now.getTime()
      )
        continue;
      const sha1 = sha1Of(attrs.certificateContent);
      if (!identities) {
        usable.push({
          id: cert.id,
          sha1,
          name: attrs.displayName || attrs.name || cert.id,
        });
        continue;
      }
      const identity = identities.find((id) => id.sha1 === sha1);
      if (identity) usable.push({ id: cert.id, sha1, name: identity.name });
    }
    const want = this.wanted;
    const chosen = want
      ? usable.filter((c) => c.sha1 === want.toUpperCase() || c.name === want)
      : usable;
    if (chosen.length === 1) return chosen[0];
    const names = usable.map((c) => c.name + " (" + c.sha1 + ")").join(", ");
    if (!chosen.length) {
      const where = identities
        ? "both in App Store Connect and in this Mac's keychain with its private key"
        : "in App Store Connect";
      throw new Error(
        (want
          ? 'No distribution certificate "' + want + '" is '
          : "No distribution certificate is ") +
          where +
          (names ? " (usable: " + names + ")" : "") +
          ". Create an Apple Distribution certificate once (Xcode → Settings → Accounts → Manage Certificates), " +
          "or import its .p12 into the login keychain of the user the server runs as."
      );
    }
    throw new Error(
      "More than one distribution certificate could sign (" +
        names +
        "): name one under Settings → Modules → " +
        "React Native."
    );
  }

  /** { content, name, expires }. Registers the bundle ID if needed, reuses a
   * matching profile valid for over a week, else replaces it. */
  async profileFor(bundleId, certificate) {
    const q = encodeURIComponent;
    const ids = await this.api(
      "GET",
      "/v1/bundleIds?filter[identifier]=" + q(bundleId) + "&limit=200"
    );
    let bundle = (ids.data || []).find(
      (b) => (b.attributes || {}).identifier === bundleId
    );
    if (!bundle) {
      this.log("Registering the bundle ID " + bundleId + " with Apple.");
      const name = ("Feldspar " + bundleId.replace(/[.-]/g, " ")).replace(
        /[^A-Za-z0-9 ]/g,
        ""
      );
      bundle = (
        await this.api("POST", "/v1/bundleIds", {
          data: {
            type: "bundleIds",
            attributes: { identifier: bundleId, name, platform: "IOS" },
          },
        })
      ).data;
    }

    const name = profileName(bundleId);
    const found = await this.api(
      "GET",
      "/v1/profiles?filter[name]=" +
        q(name) +
        "&filter[profileType]=IOS_APP_STORE&include=bundleId,certificates&limit=200"
    );
    const weekAhead = this.now.getTime() + 7 * 24 * 60 * 60 * 1000;
    for (const profile of found.data || []) {
      const attrs = profile.attributes || {};
      const rel = profile.relationships || {};
      const good =
        attrs.profileState === "ACTIVE" &&
        new Date(attrs.expirationDate).getTime() > weekAhead &&
        ((rel.bundleId || {}).data || {}).id === bundle.id &&
        ((rel.certificates || {}).data || []).some(
          (c) => c.id === certificate.id
        );
      if (good && attrs.profileContent) {
        this.log(
          'Using the provisioning profile "' + name + '" generated earlier.'
        );
        return {
          content: attrs.profileContent,
          name,
          expires: attrs.expirationDate,
        };
      }
      this.log(
        'Deleting the stale provisioning profile "' +
          name +
          '" (' +
          profile.id +
          ")."
      );
      await this.api("DELETE", "/v1/profiles/" + profile.id);
    }

    this.log('Generating the provisioning profile "' + name + '".');
    const made = await this.api("POST", "/v1/profiles", {
      data: {
        type: "profiles",
        attributes: { name, profileType: "IOS_APP_STORE" },
        relationships: {
          bundleId: { data: { type: "bundleIds", id: bundle.id } },
          certificates: {
            data: [{ type: "certificates", id: certificate.id }],
          },
        },
      },
    });
    const attrs = made.data.attributes;
    return {
      content: attrs.profileContent,
      name,
      expires: attrs.expirationDate,
    };
  }

  /** Picks the certificate, then gets the profile (plus `certificate`). */
  async generate(bundleId, identities) {
    const certs = await this.api(
      "GET",
      "/v1/certificates?filter[certificateType]=DISTRIBUTION,IOS_DISTRIBUTION&limit=200"
    );
    const certificate = this.pickCertificate(certs.data || [], identities);
    const profile = await this.profileFor(bundleId, certificate);
    return { ...profile, certificate };
  }
}

module.exports = { IosProfileGenerator };
