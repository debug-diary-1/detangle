export default ({ command, mode }) => ({
  resolve: { alias: { "@vite-cmd": `/src/cmd/${command}`, "@vite-mode": `/src/vmode/${mode}` } },
});
