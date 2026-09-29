import starlight from "@astrojs/starlight";
import { defineConfig } from "astro/config";

const repository = "https://github.com/lemarier/kitchn";

export default defineConfig({
  site: "https://getkitchn.com",
  integrations: [
    starlight({
      title: "kitchn",
      description:
        "Portable agent workflows: named roles, narrow authority, independent review, and evidence you can check.",
      favicon: "/favicon.svg",
      logo: { src: "./src/assets/mark.svg", alt: "" },
      social: [{ icon: "github", label: "GitHub", href: repository }],
      editLink: { baseUrl: `${repository}/edit/main/apps/website/` },
      customCss: ["./src/styles/theme.css"],
      head: [
        {
          tag: "link",
          attrs: { rel: "preconnect", href: "https://fonts.gstatic.com", crossorigin: true },
        },
        {
          tag: "link",
          attrs: {
            rel: "stylesheet",
            href: "https://fonts.googleapis.com/css2?family=Big+Shoulders+Display:wght@700;900&family=Hanken+Grotesk:wght@400;500;700&family=IBM+Plex+Mono:wght@400;600&display=swap",
          },
        },
      ],
      sidebar: [
        {
          label: "Start here",
          items: [
            "docs/start/introduction",
            "docs/start/install",
            "docs/start/quickstart",
            "docs/start/glossary",
          ],
        },
        {
          label: "Concepts",
          items: ["docs/concepts/houses", "docs/concepts/roles", "docs/concepts/authority"],
        },
        {
          label: "Guides",
          items: ["docs/guides/sessions", "docs/guides/templates", "docs/guides/pins-and-recovery"],
        },
        {
          label: "Reference",
          items: ["docs/reference/cli", "docs/reference/http-backend"],
        },
        { label: "Project", items: ["docs/project/status"] },
      ],
    }),
  ],
});
