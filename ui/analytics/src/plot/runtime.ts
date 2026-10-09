// ECharts, as the Analytics UI loads it (analytics TODO A2.7): the core and
// only the chart types and components the compiler in `echarts.ts` emits, so
// what no plot uses (maps, graphs, gauges, the 3D extension…) is left out of
// the bundle. A series type the compiler starts to emit is added here too.

import { BarChart, BoxplotChart, CustomChart, HeatmapChart, LineChart, ParallelChart, ScatterChart } from "echarts/charts";
import {
  BrushComponent,
  GridComponent,
  LegendComponent,
  MarkLineComponent,
  ParallelComponent,
  PolarComponent,
  TitleComponent,
  TooltipComponent,
  VisualMapComponent,
} from "echarts/components";
import * as echarts from "echarts/core";
import { CanvasRenderer, SVGRenderer } from "echarts/renderers";

echarts.use([
  BarChart,
  BoxplotChart,
  CustomChart,
  HeatmapChart,
  LineChart,
  ParallelChart,
  ScatterChart,
  // A dashboard's plots are brushed to filter the others (A6.3).
  BrushComponent,
  GridComponent,
  LegendComponent,
  MarkLineComponent,
  ParallelComponent,
  PolarComponent,
  TitleComponent,
  TooltipComponent,
  VisualMapComponent,
  CanvasRenderer,
  // For reports (A4): printed plots are vector graphics.
  SVGRenderer,
]);

export { echarts };
