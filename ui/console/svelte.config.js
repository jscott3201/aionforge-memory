import adapter from "@sveltejs/adapter-static";

const base = process.env.AIONFORGE_CONSOLE_BASE ?? "/console";

/** @type {import("@sveltejs/kit").Config} */
const config = {
  kit: {
    adapter: adapter({
      fallback: "200.html",
    }),
    paths: {
      base: process.argv.includes("dev") ? "" : base,
    },
  },
};

export default config;
