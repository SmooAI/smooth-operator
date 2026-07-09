import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';

// A plain Vite SPA. Tailwind v4 runs as a Vite plugin (no separate config file);
// the app's only real dependency is the `@smooai/smooth-operator` SDK.
export default defineConfig({
    plugins: [react(), tailwindcss()],
    server: { port: 5273 },
});
