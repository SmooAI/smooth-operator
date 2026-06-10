import { defineConfig } from 'tsdown';

// Single ESM library build. React / react-dom stay external (peer deps) so the
// host app dedupes a single React instance — bundling React into a component
// library is the classic "invalid hook call" footgun. `@smooai/smooth-operator`
// is also external so it dedupes with the widget / any direct client use.
//
// The stylesheet (`src/styles.css`) is intentionally NOT bundled: it ships as a
// source file and is exposed via the package `exports` map as
// `@smooai/smooth-operator-react/styles.css`, so consumers opt into it with one
// import and can override every `--smooth-*` custom property it declares.
export default defineConfig({
    entry: { index: 'src/index.ts' },
    format: ['esm'],
    platform: 'neutral',
    dts: true,
    sourcemap: true,
    clean: true,
    outDir: 'dist',
    external: ['react', 'react-dom', 'react/jsx-runtime', '@smooai/smooth-operator'],
});
