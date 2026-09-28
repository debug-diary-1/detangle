declare function Component(meta: object): <T>(c: T) => T;

// Angular resolves a bare templateUrl relative to the component.
@Component({ templateUrl: "widget.html", styleUrls: ["./widget.css"] })
export class Widget {}
