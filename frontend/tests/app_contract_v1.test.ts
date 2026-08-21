import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import {
  APP_CONTRACT_NAME,
  APP_CONTRACT_VERSION,
  assertCompatibleContract,
  type ContractInfo,
  type SearchHit,
  type SearchRequest,
  type SnippetRequest,
} from "../src/app_contract_v1";

const contractRoot = fileURLToPath(new URL("../../app-contract/v1/", import.meta.url));
const loadJson = <T>(relative: string): T => JSON.parse(readFileSync(`${contractRoot}${relative}`, "utf8")) as T;

describe("App Contract v1", () => {
  it("pins the frontend to the same contract identity as the manifest", () => {
    const manifest = loadJson<{ name: string; version: number }>("contract.json");
    expect(manifest.name).toBe(APP_CONTRACT_NAME);
    expect(manifest.version).toBe(APP_CONTRACT_VERSION);
    const info = loadJson<ContractInfo>("fixtures/contract-info.json");
    expect(() => assertCompatibleContract(info)).not.toThrow();
    expect(() => assertCompatibleContract({ ...info, version: 2 })).toThrow(/Contract mismatch/);
  });

  it("keeps request/response fixture fields aligned with the canonical manifest", () => {
    const manifest = loadJson<{ dto_fields: Record<string, string[]> }>("contract.json");
    const cases: Array<[string, string]> = [
      ["SearchRequest", "fixtures/search-request.json"],
      ["SearchHit", "fixtures/search-hit.json"],
      ["IndexRequest", "fixtures/index-request.json"],
      ["SnippetRequest", "fixtures/snippet-request.json"],
      ["SnippetHit", "fixtures/snippet-hit.json"],
      ["BackgroundStatus", "fixtures/background-status.json"],
    ];
    for (const [dto, fixturePath] of cases) {
      const fixture = loadJson<Record<string, unknown>>(fixturePath);
      expect(Object.keys(fixture).sort(), dto).toEqual([...manifest.dto_fields[dto]].sort());
    }
  });

  it("type-checks representative contract fixtures against frontend DTOs", () => {
    const request: SearchRequest = loadJson("fixtures/search-request.json");
    const hit: SearchHit = loadJson("fixtures/search-hit.json");
    const snippet: SnippetRequest = loadJson("fixtures/snippet-request.json");
    expect(request.limit).toBe(2_000);
    expect(hit.content_state).toBe("indexed");
    expect(snippet.maxHits).toBe(10);
  });
});
