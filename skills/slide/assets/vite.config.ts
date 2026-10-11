const cacheDir = process.env.SLIDEV_CACHE_DIR;

if (!cacheDir) {
  throw new Error("SLIDEV_CACHE_DIR is required");
}

export default { cacheDir };
