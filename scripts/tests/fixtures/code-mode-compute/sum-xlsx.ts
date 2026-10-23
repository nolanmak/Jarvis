async function main() {
  const request = {
    runtime: "python",
    dependencies: ["openpyxl==3.1.5"],
    inputs: [{ artifactId: computeInputs.sheet, name: "numbers.xlsx" }],
    code: `import json
from openpyxl import load_workbook
book = load_workbook('/inputs/numbers.xlsx', read_only=True, data_only=True)
values = [row[0] for row in book.active.iter_rows(values_only=True)]
summary = {'count': len(values), 'total': sum(values)}
print(json.dumps(summary))
`,
  };
  const cold = await tools.compute.run(request);
  if (!cold.ok || cold.runner !== "vm" || cold.environmentReused || cold.dependencyLock.length < 2) {
    throw new Error("Cold public-package execution failed");
  }
  const warm = await tools.compute.run({
    ...request,
    outputs: ["summary.json"],
    code: request.code + "open('/outputs/summary.json', 'w').write(json.dumps(summary))\n",
  });
  if (!warm.ok || warm.runner !== "vm" || !warm.environmentReused) {
    throw new Error("Warm environment was not reused");
  }
  const result = JSON.parse(warm.stdout);
  if (result.count !== 3 || result.total !== 60) throw new Error("Wrong spreadsheet total");
  return result;
}
main();
