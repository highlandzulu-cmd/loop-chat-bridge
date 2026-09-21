import tailwindcss from "@tailwindcss/vite";
import { defineConfig } from "vite";

export default defineConfig({
	plugins: [tailwindcss()],
	server: {
		port: 5173,
		// Fail loudly if 5173 is taken instead of silently hopping to 5174,
		// which the bridge's CORS allow-list doesn't cover — that used to show
		// up as a baffling "Failed to fetch" on every request.
		strictPort: true,
	},
	// pi-web-ui's PDF attachment handling sets up the pdfjs-dist worker via
	// `new URL("pdfjs-dist/build/pdf.worker.min.mjs", import.meta.url)`.
	// Hit live: "Setting up fake worker failed" the first time a PDF was
	// attached in a dev session — a known class of issue where Vite's
	// on-demand dependency pre-bundling (triggered the first time a new dep
	// is actually exercised, mid-session) races with that worker import.
	// This is the standard fix: exclude pdfjs-dist from pre-bundling so it's
	// always served straight from node_modules, sidestepping that race
	// rather than timing around it. Not independently reproduced under
	// controlled conditions here — timing-dependent and specific to a live
	// upload through the file picker, which couldn't be scripted — so treat
	// this as the correct standard fix for the documented error, confirmed
	// by actually attaching a PDF in the running app, not as something
	// proven from first principles in isolation.
	optimizeDeps: {
		exclude: ["pdfjs-dist"],
	},
});
