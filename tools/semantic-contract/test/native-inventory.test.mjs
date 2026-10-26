import test from "node:test";
import assert from "node:assert/strict";
import {
  inventoryMismatch,
  productionInventoryEntries,
  trustedInventory,
} from "../native-inventory.mjs";

const producer = "trellis.native.syntax";
const version = "native-v10";
const languages = ["java", "rust", "python", "javascript"];

test("NATIVE.INVENTORY production descriptor declares each supported language", () => {
  assert.deepEqual(productionInventoryEntries(), [
    [producer, version, languages.map((language) => [language, []])],
  ]);
  for (const language of languages) {
    assert.deepEqual(trustedInventory(producer, version, language), []);
    assert.equal(inventoryMismatch(producer, version, language, []), null);
  }
});

test("NATIVE.INVENTORY changed claims cannot redeclare an existing version", () => {
  for (const language of languages) {
    for (const changed of [["config"], ["toolchain"], ["dependency"], null]) {
      assert.match(
        inventoryMismatch(producer, version, language, changed),
        /declared inventory differs/,
      );
      assert.deepEqual(trustedInventory(producer, version, language), []);
    }
  }
});

test("NATIVE.INVENTORY unknown producer, future version and language fail closed", () => {
  for (const [id, descriptorVersion, language] of [
    ["unknown.native", version, "rust"],
    [producer, "native-unregistered-future", "rust"],
    [producer, "native-v3", "rust"],
    [producer, "native-v4", "rust"],
    [producer, version, "go"],
  ]) {
    assert.equal(trustedInventory(id, descriptorVersion, language), undefined);
    for (const declared of [[], ["config"]]) {
      assert.match(
        inventoryMismatch(id, descriptorVersion, language, declared),
        /no trusted inventory/,
      );
    }
  }
});
