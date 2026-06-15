// plotly.js-dist-min ships no types; it is the same runtime object as plotly.js.
declare module "plotly.js-dist-min" {
  const Plotly: typeof import("plotly.js");
  export default Plotly;
}
