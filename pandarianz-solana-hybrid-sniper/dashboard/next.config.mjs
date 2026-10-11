// The browser talks only to Next; /engine/* is proxied to the engine's read-only GET API.
const engine = process.env.SNIPER_API_URL || `http://${process.env.SNIPER_API_ADDR || "127.0.0.1:8787"}`;
/** @type {import('next').NextConfig} */
const nextConfig = {
  reactStrictMode: true,
  async rewrites() {
    return [{ source: "/engine/:path*", destination: `${engine}/api/:path*` }];
  },
};
export default nextConfig;
