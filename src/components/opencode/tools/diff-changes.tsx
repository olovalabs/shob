type Change = { additions: number; deletions: number }

export function DiffChanges({
  changes,
  variant = "default",
  className,
}: {
  changes: Change | Change[]
  variant?: "default" | "bars"
  className?: string
}) {
  const list = Array.isArray(changes) ? changes : [changes]
  const total = list.reduce(
    (acc, c) => ({
      additions: acc.additions + c.additions,
      deletions: acc.deletions + c.deletions,
    }),
    { additions: 0, deletions: 0 },
  )

  if (variant === "bars") {
    return <DiffBars changes={list} className={className} />
  }

  return (
    <div className={className} data-slot="diff-changes" data-variant="default">
      <span data-slot="diff-changes-additions">+{total.additions}</span>
      <span data-slot="diff-changes-deletions">-{total.deletions}</span>
    </div>
  )
}

function DiffBars({ changes, className }: { changes: Change[]; className?: string }) {
  const total = changes.reduce(
    (acc, c) => ({
      additions: acc.additions + c.additions,
      deletions: acc.deletions + c.deletions,
    }),
    { additions: 0, deletions: 0 },
  )
  const totalChanges = total.additions + total.deletions
  const BARS = 5

  const bars = Array.from({ length: BARS }, (_, i) => {
    if (totalChanges === 0) return { type: "neutral" as const, ratio: 1 }
    const pos = (i + 0.5) / BARS
    const cumulativeA = total.additions / totalChanges
    if (pos <= cumulativeA) return { type: "add" as const, ratio: 1 / Math.max(BARS * cumulativeA, 1) }
    return { type: "delete" as const, ratio: 1 / Math.max(BARS * (1 - cumulativeA), 1) }
  })

  return (
    <svg viewBox="0 0 18 14" data-slot="diff-changes-bars" className={className}>
      {bars.map((bar, i) => (
        <rect
          key={i}
          x={1 + i * 3.4}
          y={1}
          width="2.4"
          height="12"
          rx="1.2"
          fill={
            bar.type === "add"
              ? "var(--icon-diff-add-base)"
              : bar.type === "delete"
                ? "var(--icon-diff-delete-base)"
                : "var(--icon-weak-base)"
          }
          style={{ transform: `scaleY(${bar.ratio})`, transformOrigin: "9px 13px" }}
        />
      ))}
    </svg>
  )
}
