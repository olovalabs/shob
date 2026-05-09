export function stripWorkingDirectory(filePath?: string, workingDir?: string) {
  if (filePath === undefined || workingDir === undefined) return filePath
  const prefix = workingDir.endsWith("/") ? workingDir : workingDir + "/"
  if (filePath === workingDir) return ""
  if (filePath.startsWith(prefix)) return filePath.slice(prefix.length)
  return filePath
}

export function formatErrorString(error: string): string {
  const errorMarker = "Error: "
  const startsWithError = error.startsWith(errorMarker)
  return startsWithError ? error.slice(errorMarker.length) : error
}