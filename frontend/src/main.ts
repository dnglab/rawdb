import { createApp } from 'vue';
import { createPinia } from 'pinia';
import PrimeVue from 'primevue/config';
import { definePreset } from '@primevue/themes';
import Aura from '@primevue/themes/aura';
import ToastService from 'primevue/toastservice';
import ConfirmationService from 'primevue/confirmationservice';
import 'primeicons/primeicons.css';
import './styles.css';

import Button from 'primevue/button';
import InputText from 'primevue/inputtext';
import Password from 'primevue/password';
import InputNumber from 'primevue/inputnumber';
import Select from 'primevue/select';
import Textarea from 'primevue/textarea';
import DataTable from 'primevue/datatable';
import Column from 'primevue/column';
import Card from 'primevue/card';
import Tag from 'primevue/tag';
import Message from 'primevue/message';
import ProgressBar from 'primevue/progressbar';
import ProgressSpinner from 'primevue/progressspinner';
import Skeleton from 'primevue/skeleton';
import Toast from 'primevue/toast';
import ConfirmDialog from 'primevue/confirmdialog';
import IconField from 'primevue/iconfield';
import InputIcon from 'primevue/inputicon';
import Panel from 'primevue/panel';
import FileUpload from 'primevue/fileupload';
import Checkbox from 'primevue/checkbox';
import Divider from 'primevue/divider';
import Chip from 'primevue/chip';
import AutoComplete from 'primevue/autocomplete';
import Menubar from 'primevue/menubar';
import Avatar from 'primevue/avatar';

import App from './App.vue';
import { router } from './router';

// Indigo accent on neutral slate surfaces, light mode only.
const RawdbPreset = definePreset(Aura, {
  semantic: {
    primary: {
      50: '{indigo.50}',
      100: '{indigo.100}',
      200: '{indigo.200}',
      300: '{indigo.300}',
      400: '{indigo.400}',
      500: '{indigo.500}',
      600: '{indigo.600}',
      700: '{indigo.700}',
      800: '{indigo.800}',
      900: '{indigo.900}',
      950: '{indigo.950}',
    },
    colorScheme: {
      light: {
        surface: {
          0: '#ffffff',
          50: '{slate.50}',
          100: '{slate.100}',
          200: '{slate.200}',
          300: '{slate.300}',
          400: '{slate.400}',
          500: '{slate.500}',
          600: '{slate.600}',
          700: '{slate.700}',
          800: '{slate.800}',
          900: '{slate.900}',
          950: '{slate.950}',
        },
      },
    },
  },
});

const app = createApp(App);
app.use(createPinia());
app.use(router);
app.use(PrimeVue, {
  theme: { preset: RawdbPreset, options: { darkModeSelector: false } },
});
app.use(ToastService);
app.use(ConfirmationService);

app.component('Button', Button);
app.component('InputText', InputText);
app.component('Password', Password);
app.component('InputNumber', InputNumber);
app.component('Select', Select);
app.component('Textarea', Textarea);
app.component('DataTable', DataTable);
app.component('Column', Column);
app.component('Card', Card);
app.component('Tag', Tag);
app.component('Message', Message);
app.component('ProgressBar', ProgressBar);
app.component('ProgressSpinner', ProgressSpinner);
app.component('Skeleton', Skeleton);
app.component('Toast', Toast);
app.component('ConfirmDialog', ConfirmDialog);
app.component('IconField', IconField);
app.component('InputIcon', InputIcon);
app.component('Panel', Panel);
app.component('FileUpload', FileUpload);
app.component('Checkbox', Checkbox);
app.component('Divider', Divider);
app.component('Chip', Chip);
app.component('AutoComplete', AutoComplete);
app.component('Menubar', Menubar);
app.component('Avatar', Avatar);

app.mount('#app');
