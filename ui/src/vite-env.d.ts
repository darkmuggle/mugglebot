/// <reference types="vite/client" />

interface ImportMetaEnv {
  /** Backend host:port, injected by Tilt from config.toml's [ui].listen. */
  readonly VITE_BACKEND?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
