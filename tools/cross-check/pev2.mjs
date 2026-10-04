// Prints every node of a plan with pev2's exclusive time, in the format
// compare.py reads. pev2 does not export its plan service, so this finds the
// service object in the published bundle and exports it from a copy.
//
//   node pev2.mjs node_modules/pev2/dist/pev2.es.js <plan file>
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { pathToFileURL } from "node:url";

const [bundlePath, planPath] = process.argv.slice(2);
const bundle = readFileSync(bundlePath, "utf8");
// The service is a singleton: `<name> = new class { ... createPlan(`.
const service = [...bundle.matchAll(/([A-Za-z_$][\w$]*) = new class \{/g)].find((match) => {
  const start = match.index + match[0].length;
  const next = bundle.indexOf("createPlan(", start);
  const following = bundle.indexOf(" = new class {", start);
  return next > 0 && (following < 0 || next < following);
});
if (!service) throw new Error("plan service not found in the pev2 bundle");
// Next to the bundle, so that its imports resolve the same way.
const copy = join(dirname(bundlePath), "pev2-with-service.mjs");
writeFileSync(copy, `${bundle}\nexport { ${service[1]} as planService };\n`);
const { planService } = await import(pathToFileURL(copy).href);

const plan = planService.createPlan("plan", planService.fromSource(readFileSync(planPath, "utf8")), "");
// CTEs are kept apart from the tree.
const queue = [plan.content.Plan, ...(plan.ctes || [])];
while (queue.length) {
  const node = queue.shift();
  console.log(
    [
      node["Actual Total Time"] ?? "",
      node["Actual Rows"] ?? "",
      node["Actual Loops"] ?? "",
      node["*Duration (exclusive)"] ?? -1,
      -1,
      node["Node Type"],
    ].join("\t"),
  );
  queue.push(...(node.Plans || []));
}
