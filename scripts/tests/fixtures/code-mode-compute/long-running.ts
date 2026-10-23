async function main() {
  const result = await tools.compute.run({
    runtime: "python", dependencies: [], timeoutSecs: 90,
    code: "import time\ntime.sleep(65)\nprint('completed')",
  });
  if (!result.ok || result.runner !== "vm" || result.stdout.trim() !== "completed") {
    throw new Error("Compute did not survive the previous 60-second ceiling");
  }
  return { completed: true };
}
main();
