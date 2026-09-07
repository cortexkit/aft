import {
  getAftBinaryCacheDir,
  getAftLspBinariesDir,
  getAftLspPackagesDir,
  resolveCortexKitStorageRoot,
  type StoragePathContext,
} from "@cortexkit/aft-bridge";

export { getAftBinaryCacheDir, getAftLspBinariesDir, getAftLspPackagesDir };

export function getAftBinaryName(): string {
  return process.platform === "win32" ? "aft.exe" : "aft";
}

export function getCortexKitStorageRoot(context: StoragePathContext = {}): string {
  return resolveCortexKitStorageRoot(context);
}
