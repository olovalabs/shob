import { clsx, type ClassValue } from "clsx"
import { twMerge } from "tailwind-merge"

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}

export function getFilename(path: string): string {
  if (!path) return ""
  const parts = path.replace(/[\\/]+$/, "").split(/[\\/]/)
  return parts[parts.length - 1] || path
}
