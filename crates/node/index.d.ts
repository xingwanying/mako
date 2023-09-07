/* tslint:disable */
/* eslint-disable */

/* auto-generated by NAPI-RS */

export function build(root: string, config:
{
entry?: Record<string, string>;
output?: {path: string; mode: "bundle" | "minifish" ;  esVersion?: string, };
resolve?: {
alias?: Record<string, string>;
extensions?: string[];
};
manifest?: boolean;
manifest_config?: {
file_name: string;
base_path: string;
};
mode?: "development" | "production";
define?: Record<string, string>;
devtool?: "source-map" | "inline-source-map" | "none";
externals?: Record<string, string>;
copy?: string[];
code_splitting: "bigVendors" | "depPerChunk" | "none";
providers?: Record<string, string[]>;
public_path?: string;
inline_limit?: number;
targets?: Record<string, number>;
platform?: "node" | "browser";
hmr?: boolean;
hmr_port?: string;
hmr_host?: string;
stats?: boolean;
}, watch: boolean): Promise<void>
