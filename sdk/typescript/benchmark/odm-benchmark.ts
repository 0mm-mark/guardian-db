import assert from "node:assert/strict";
import { performance } from "node:perf_hooks";

import GuardianDB, {
  DuplicateKeyError,
  MemoryTransport,
  defineSchema,
  type Document,
} from "../src/index.js";

interface BenchmarkDocument extends Document {
  id?: string;
  email: string;
  tenant: string;
  group: string;
  counter: number;
  payload: string;
  metadata: { active?: boolean; bucket?: number; region?: string; [key: string]: unknown };
  createdAt?: string;
  updatedAt?: string;
}

type Mode = "insert" | "query" | "update" | "large" | "reliability" | "runAll";

interface Options {
  mode: Mode;
  docs: number;
  batchSize: number;
  queries: number;
  updates: number;
  payloadBytes: number;
  largeMb: number;
  includeLarge: boolean;
}

interface Metric {
  name: string;
  operations: number;
  elapsedMs: number;
  opsPerSecond: number;
  details?: Record<string, unknown>;
}

const schema = defineSchema<BenchmarkDocument>({
  strict: true,
  timestamps: { createdAt: "createdAt", updatedAt: "updatedAt" },
  fields: {
    id: { type: String, primaryKey: true },
    email: { type: String, required: true, unique: true },
    tenant: { type: String, required: true, index: true },
    group: { type: String, required: true, index: true },
    counter: { type: Number, required: true },
    payload: { type: String, required: true },
    metadata: { type: Object, required: true },
    createdAt: { type: "timestamp" },
    updatedAt: { type: "timestamp" },
  },
});

async function main(): Promise<void> {
  const options = parseOptions(process.argv.slice(2));
  const metrics: Metric[] = [];

  if (options.mode === "insert" || options.mode === "runAll") {
    metrics.push(await benchmarkInsert(options));
  }
  if (options.mode === "query" || options.mode === "runAll") {
    metrics.push(...await benchmarkQueries(options));
  }
  if (options.mode === "update" || options.mode === "runAll") {
    metrics.push(await benchmarkUpdates(options));
  }
  if (options.mode === "reliability" || options.mode === "runAll") {
    metrics.push(await benchmarkReliability(options));
  }
  if (options.mode === "large" || (options.mode === "runAll" && options.includeLarge)) {
    metrics.push(await benchmarkLargeDocument(options));
  }

  printSummary(metrics, options);
}

async function benchmarkInsert(options: Options): Promise<Metric> {
  const employees = await freshCollection("bench_insert");
  const batches = chunkedDocuments(options.docs, options.payloadBytes, options.batchSize);

  return time("insert.batch", options.docs, async () => {
    for (const batch of batches) {
      await employees.insert(batch);
    }
    assert.equal((await employees.find({})).length, options.docs);
  }, { batchSize: options.batchSize, payloadBytes: options.payloadBytes });
}

async function benchmarkQueries(options: Options): Promise<Metric[]> {
  const employees = await freshCollection("bench_query");
  for (const batch of chunkedDocuments(options.docs, options.payloadBytes, options.batchSize)) {
    await employees.insert(batch);
  }

  const byId = await time("query.findById", options.queries, async () => {
    for (let i = 0; i < options.queries; i += 1) {
      const index = i % options.docs;
      const found = await employees.findById(id(index));
      assert.equal(found?.email, email(index));
    }
  }, { docs: options.docs });

  const uniqueIndex = await time("query.findOne.uniqueIndex", options.queries, async () => {
    for (let i = 0; i < options.queries; i += 1) {
      const index = i % options.docs;
      const found = await employees.findOne({ email: email(index) });
      assert.equal(found?.id, id(index));
    }
  }, { docs: options.docs });

  const secondaryIndex = await time("query.find.secondaryIndex", options.queries, async () => {
    for (let i = 0; i < options.queries; i += 1) {
      const group = `group-${i % 64}`;
      const found = await employees.find({ group });
      assert.ok(found.length > 0);
    }
  }, { docs: options.docs });

  return [byId, uniqueIndex, secondaryIndex];
}

async function benchmarkUpdates(options: Options): Promise<Metric> {
  const employees = await freshCollection("bench_update");
  for (const batch of chunkedDocuments(options.docs, options.payloadBytes, options.batchSize)) {
    await employees.insert(batch);
  }

  return time("update.$set.$inc.uniqueIndex", options.updates, async () => {
    for (let i = 0; i < options.updates; i += 1) {
      const index = i % options.docs;
      const updated = await employees.update(
        { email: email(index) },
        { $set: { tenant: "tenant-hot", "metadata.status": "updated" }, $inc: { counter: 1 } },
      );
      assert.equal(updated?.tenant, "tenant-hot");
    }
  }, { docs: options.docs });
}

async function benchmarkReliability(options: Options): Promise<Metric> {
  const employees = await freshCollection("bench_reliability");
  for (const batch of chunkedDocuments(options.docs, options.payloadBytes, options.batchSize)) {
    await employees.insert(batch);
  }

  return time("reliability.unique.rollback", 3, async () => {
    await assert.rejects(
      employees.insert([
        document(options.docs + 1, options.payloadBytes, { idOverride: "duplicate-a", emailOverride: "duplicate@example.test" }),
        document(options.docs + 2, options.payloadBytes, { idOverride: "duplicate-b", emailOverride: "duplicate@example.test" }),
      ]),
      DuplicateKeyError,
    );
    assert.equal(await employees.findById("duplicate-a"), null);
    assert.equal(await employees.findById("duplicate-b"), null);

    const concurrent = await Promise.allSettled([
      employees.insertOne(document(options.docs + 10, options.payloadBytes, { emailOverride: "race@example.test" })),
      employees.insertOne(document(options.docs + 11, options.payloadBytes, { emailOverride: "race@example.test" })),
      employees.insertOne(document(options.docs + 12, options.payloadBytes, { emailOverride: "race@example.test" })),
    ]);
    assert.equal(concurrent.filter((result) => result.status === "fulfilled").length, 1);
    assert.equal(concurrent.filter((result) => result.status === "rejected").length, 2);
  }, { docs: options.docs });
}

async function benchmarkLargeDocument(options: Options): Promise<Metric> {
  const employees = await freshCollection("bench_large");
  const payloadBytes = options.largeMb * 1024 * 1024;

  return time("large.insert.find.update", 3, async () => {
    const inserted = await employees.insertOne(document(0, payloadBytes));
    assert.equal(inserted.payload.length, payloadBytes);

    const found = await employees.findById(id(0));
    assert.equal(found?.payload.length, payloadBytes);

    const updated = await employees.update(
      { id: id(0) },
      { $set: { "metadata.largeDocRoundTrip": true } },
    );
    assert.equal(updated?.metadata.largeDocRoundTrip, true);
  }, { payloadBytes, note: "set --large-mb=17+ to probe above MongoDB's 16 MiB BSON limit" });
}

async function freshCollection(name: string) {
  MemoryTransport.shared.reset();
  const db = await GuardianDB.init(`benchmark_${name}_${Date.now()}_${Math.random().toString(36).slice(2)}`, {});
  return db.initCollection<BenchmarkDocument>("benchmark_documents", { schema });
}

function chunkedDocuments(count: number, payloadBytes: number, batchSize: number): BenchmarkDocument[][] {
  const batches: BenchmarkDocument[][] = [];
  for (let start = 0; start < count; start += batchSize) {
    const length = Math.min(batchSize, count - start);
    batches.push(Array.from({ length }, (_, offset) => document(start + offset, payloadBytes)));
  }
  return batches;
}

function document(
  index: number,
  payloadBytes: number,
  overrides: { idOverride?: string; emailOverride?: string } = {},
): BenchmarkDocument {
  return {
    id: overrides.idOverride ?? id(index),
    email: overrides.emailOverride ?? email(index),
    tenant: `tenant-${index % 16}`,
    group: `group-${index % 64}`,
    counter: index,
    payload: payload(payloadBytes, index),
    metadata: {
      active: index % 2 === 0,
      bucket: index % 128,
      region: `region-${index % 8}`,
    },
  };
}

function id(index: number): string {
  return `bench-${index.toString().padStart(10, "0")}`;
}

function email(index: number): string {
  return `${id(index)}@example.test`;
}

function payload(size: number, seed: number): string {
  const pattern = `guardian-db-odm-benchmark-${seed.toString(16).padStart(16, "0")}-`;
  let value = "";
  while (value.length < size) value += pattern;
  return value.slice(0, size);
}

async function time(
  name: string,
  operations: number,
  run: () => Promise<void>,
  details?: Record<string, unknown>,
): Promise<Metric> {
  const start = performance.now();
  await run();
  const elapsedMs = performance.now() - start;
  const metric: Metric = {
    name,
    operations,
    elapsedMs,
    opsPerSecond: operations / (elapsedMs / 1_000),
  };
  if (details !== undefined) {
    metric.details = details;
  }
  return metric;
}

function parseOptions(args: string[]): Options {
  const values = new Map<string, string | boolean>();
  for (const arg of args) {
    if (!arg.startsWith("--")) continue;
    const [rawKey, rawValue] = arg.slice(2).split("=", 2);
    if (rawKey === undefined || rawKey.length === 0) continue;
    values.set(rawKey, rawValue ?? true);
  }

  const mode = stringValue(values, "mode", "runAll") as Mode;
  const allowedModes = new Set<Mode>(["insert", "query", "update", "large", "reliability", "runAll"]);
  if (!allowedModes.has(mode)) {
    throw new Error(`Unsupported --mode=${mode}; expected one of ${[...allowedModes].join(", ")}`);
  }

  return {
    mode,
    docs: numberValue(values, "docs", 5_000),
    batchSize: numberValue(values, "batch-size", 1_000),
    queries: numberValue(values, "queries", 1_000),
    updates: numberValue(values, "updates", 1_000),
    payloadBytes: numberValue(values, "payload-bytes", 512),
    largeMb: numberValue(values, "large-mb", 17),
    includeLarge: booleanValue(values, "include-large", false),
  };
}

function stringValue(values: Map<string, string | boolean>, key: string, fallback: string): string {
  const value = values.get(key);
  return typeof value === "string" && value.length > 0 ? value : fallback;
}

function numberValue(values: Map<string, string | boolean>, key: string, fallback: number): number {
  const value = values.get(key);
  if (typeof value !== "string") return fallback;
  const parsed = Number(value);
  if (!Number.isSafeInteger(parsed) || parsed <= 0) {
    throw new Error(`--${key} must be a positive integer`);
  }
  return parsed;
}

function booleanValue(values: Map<string, string | boolean>, key: string, fallback: boolean): boolean {
  const value = values.get(key);
  if (value === undefined) return fallback;
  if (value === true) return true;
  if (value === false) return false;
  return ["1", "true", "yes", "on"].includes(value.toLowerCase());
}

function printSummary(metrics: Metric[], options: Options): void {
  console.log(JSON.stringify({
    engine: "GuardianDB TypeScript ODM MemoryTransport",
    options,
    metrics: metrics.map((metric) => ({
      ...metric,
      elapsedMs: Number(metric.elapsedMs.toFixed(3)),
      opsPerSecond: Number(metric.opsPerSecond.toFixed(3)),
    })),
  }, null, 2));
}

main().catch((error: unknown) => {
  console.error(error);
  process.exitCode = 1;
});
