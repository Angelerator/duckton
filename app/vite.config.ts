import { sveltekit } from '@sveltejs/kit/vite';
import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';

// @tauri-apps/cli sets TAURI_DEV_HOST when running on a device; harmless on desktop.
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
	plugins: [tailwindcss(), sveltekit()],
	// Tauri expects a fixed port and surfaces Rust errors clearly.
	clearScreen: false,
	server: {
		port: 1420,
		strictPort: true,
		host: host || false,
		hmr: host ? { protocol: 'ws', host, port: 1421 } : undefined,
		watch: {
			// Don't reload the frontend when the Rust backend changes.
			ignored: ['**/src-tauri/**']
		}
	}
});
