import { createApp } from "vue";
import App from "./App.vue";
import Button from "./components/Button.svelte";
import { AppComponent } from "./app/app.component";
createApp(App);
export { Button, AppComponent };
