/** Self-hosted Tailwind build (replaces the deprecated Play CDN that the
 *  dashboard previously loaded at runtime). Output → public/tailwind.css,
 *  served by Express static at /tailwind.css. See views/partials/header.ejs.
 */
module.exports = {
  content: ["./views/**/*.ejs"],
  darkMode: "class",
  theme: {
    extend: {
      colors: {
        surface: { DEFAULT: "#1a1a2e", light: "#16213e", card: "#0f3460" },
        accent: { DEFAULT: "#e94560", soft: "#533483" },
      },
    },
  },
  plugins: [],
};
