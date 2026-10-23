import { test } from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { readFileSync } from "node:fs";

const require = createRequire(import.meta.url);
const { ADAPTERS, adapterFor, hosts } = require("../adapters/index.js");
const { can, requireCapability, checkDeclaration, CAPABILITIES } = require("../capabilities.js");

test("every adapter declares known capabilities and its hosts", () => {
  for (const [name, adapter] of Object.entries(ADAPTERS)) {
    assert.equal(adapter.name, name);
    assert.deepEqual(checkDeclaration(adapter), [], name);
    for (const method of ["client", "createCheckout", "verifyWebhook", "fetch", "list"]) {
      assert.equal(typeof adapter[method], "function", `${name}.${method}`);
    }
  }
});

test("the module may reach exactly the hosts its adapters name", () => {
  const manifest = JSON.parse(readFileSync(new URL("../feldspar-module.json", import.meta.url)));
  assert.deepEqual([...manifest.permissions.net].sort(), hosts());
});

test("an account that cannot be used is refused, saying why", () => {
  const account = (fields) => ({ name: "shop", adapter: "fake", mode: "test", enabled: true, ...fields });
  assert.equal(adapterFor(account()).name, "fake");
  assert.throws(() => adapterFor(account({ adapter: "acme" })), /names the adapter "acme", which this module does not have; it has: fake/);
  assert.throws(() => adapterFor(account({ mode: "live" })), /takes no real money, so it cannot be in live mode/);
  assert.throws(() => adapterFor(account({ mode: "prod" })), /mode "prod"; it is test or live/);
  assert.throws(() => adapterFor(account({ enabled: false })), /is disabled/);
});

test("a capability the adapter lacks is refused by name", () => {
  const fake = ADAPTERS.fake;
  assert.equal(can(fake, "one_off"), true);
  assert.equal(can(fake, "subscriptions"), true);
  assert.equal(can(fake, "charge_saved_method"), true);
  assert.equal(can(fake, "native_sheet"), false);
  requireCapability({ name: "shop" }, fake, "one_off");
  assert.throws(
    () => requireCapability({ name: "shop" }, fake, "manual_capture"),
    /^Error: the fake account "shop" cannot [^:]+: its adapter does not support it$/,
  );
  assert.throws(() => can(fake, "teleport"), /not a payment capability/);
  assert.ok(Object.keys(CAPABILITIES).length >= 10);
});
