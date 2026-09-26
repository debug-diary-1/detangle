import { Component } from "@angular/core";

@Component({
  selector: "app-root",
  templateUrl: "./app.component.html",
  styleUrl: "missing.component.css",
})
export class AppComponent {
  static routes = [{ path: "x", loadComponent: () => import("../util").then((m) => m.format) }];
}
