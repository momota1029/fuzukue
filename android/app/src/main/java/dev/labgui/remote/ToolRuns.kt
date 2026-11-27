package dev.labgui.remote

private class ToolRun {
    val tools = linkedMapOf<String, TimelineRow.Tool>()
    val attached = mutableMapOf<String, Int>()
    var unlinked = 0
    val root: String get() = tools.keys.first()
    val grouped: Boolean get() = attached.isNotEmpty() || unlinked > 0
}

internal fun groupToolRows(rows: Map<String, TimelineRow>, receipts: Collection<ApprovalReceipt>): Map<String, TimelineRow> {
    val runs = mutableMapOf<String, ToolRun>()
    var current: ToolRun? = null
    // [rows] holds visible rows only, so a hidden turn never splits a run.
    for ((id, row) in rows) {
        if (row is TimelineRow.Tool) {
            val run = current ?: ToolRun()
            run.tools[id] = row
            runs[id] = run
            current = run
        } else {
            current = null
        }
    }

    val standalone = mutableMapOf<String, Int>()
    val denials = mutableMapOf<String, MutableList<ApprovalReceipt>>()
    for (receipt in receipts.sortedWith(compareBy({ it.updatedAt }, { it.id }))) {
        if (receipt.workerJob != null) continue
        val original = receipt.anchor ?: continue
        val anchor = listOf(original, "user/$original").firstOrNull { it in rows } ?: continue
        val run = runs[anchor]
        if (receipt.state == ApprovalQueueState.Denied) {
            denials.getOrPut(run?.root ?: anchor) { mutableListOf() }.add(receipt)
        } else if (receipt.automatic) {
            when {
                run == null -> standalone[anchor] = (standalone[anchor] ?: 0) + 1
                receipt.toolItem == anchor -> run.attached[anchor] = (run.attached[anchor] ?: 0) + 1
                else -> run.unlinked++
            }
        }
    }

    // A count with only tools after it joins their summary instead of adding a second one.
    val ids = rows.keys.toList()
    for ((anchor, next) in ids.zipWithNext()) {
        val run = runs[next] ?: continue
        if (anchor in denials) continue
        run.unlinked += standalone.remove(anchor) ?: continue
    }

    return buildMap {
        for ((id, row) in rows) {
            val run = runs[id]
            if (run == null || !run.grouped) {
                put(id, row)
            } else if (id == run.root) {
                put(id, TimelineRow.ToolRun(
                    run.tools.map { (item, tool) -> tool to (run.attached[item] ?: 0) },
                    run.unlinked,
                ))
            }
            standalone[id]?.let { count -> put("approval-count/$id", TimelineRow.ToolRun(emptyList(), count)) }
            for (denial in denials[id].orEmpty()) {
                put("approval-denial/${denial.id}", TimelineRow.ApprovalDenial(denial.tool))
            }
        }
    }
}
