// The docs pages in reading order. The sidebar, the prev/next links and the
// sitemap order all come from this list.
export const docs = [
  { group: "Start", slug: "", title: "Overview" },
  { group: "Start", slug: "install", title: "Install and log in" },
  { group: "Use", slug: "sessions", title: "Sessions" },
  { group: "Use", slug: "forward-exec", title: "Forwards and exec" },
  { group: "Host", slug: "server", title: "Running the server" },
  { group: "Host", slug: "devices", title: "Devices and keys" },
  { group: "Reference", slug: "cli", title: "Command line" },
  { group: "Reference", slug: "files", title: "Files and environment" },
  { group: "Reference", slug: "troubleshooting", title: "Troubleshooting" },
  { group: "Compare", slug: "vs-ssh", title: "osewa vs ssh" },
  { group: "Compare", slug: "vs-mosh", title: "osewa vs mosh" },
  { group: "Compare", slug: "vs-et", title: "osewa vs ET" },
];

export const docPath = (slug) => (slug ? `/docs/${slug}` : "/docs");
