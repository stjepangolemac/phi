import { batch, log, parallel, phase } from "phi:workflow"

export const meta = {
  name: "scheduling-example",
  description: "Demonstrate limited parallelism and fixed-size batch barriers."
}

function tracker() {
  let running = 0
  let maximum = 0
  const events = []

  return {
    task(label, wait = null, release = null) {
      return async () => {
        running += 1
        maximum = Math.max(maximum, running)
        events.push(`start:${label}`)
        if (release) release()
        if (wait) await wait
        events.push(`end:${label}`)
        running -= 1
        return label
      }
    },
    summary() {
      return { maximum, events }
    }
  }
}

export default async function () {
  phase("Limited parallelism")
  let releaseParallelFirst
  const parallelFirst = new Promise(resolve => { releaseParallelFirst = resolve })
  const parallelTracker = tracker()
  const parallelResults = await parallel([
    parallelTracker.task("p1", parallelFirst),
    parallelTracker.task("p2"),
    parallelTracker.task("p3", null, releaseParallelFirst),
    parallelTracker.task("p4")
  ], { concurrency: 2 })
  log("Parallel tasks completed")

  phase("Fixed-size batches")
  let releaseBatchFirst
  const batchFirst = new Promise(resolve => { releaseBatchFirst = resolve })
  const batchTracker = tracker()
  const batchResults = await batch([
    batchTracker.task("b1", batchFirst),
    batchTracker.task("b2", null, releaseBatchFirst),
    batchTracker.task("b3"),
    batchTracker.task("b4")
  ], { size: 2 })
  log("Batch tasks completed")

  return {
    parallel: {
      results: parallelResults,
      ...parallelTracker.summary()
    },
    batch: {
      results: batchResults,
      ...batchTracker.summary()
    }
  }
}
